//! Unix socket（`SOCK_SEQPACKET`）の小さなヘルパ。
//!
//! - ディレクトリは 0700、socket は 0600 にする。
//! - 接続してきたプロセスの uid を `SO_PEERCRED` で検証する。
//! - 1 接続 1 メッセージ。読み取りにはタイムアウトを付ける。

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::socket::{
    Backlog, MsgFlags, SockFlag, SockType, UnixAddr, accept4, bind, connect as nix_connect,
    getsockopt, listen, recv, send, setsockopt, socket, sockopt,
};
use nix::sys::time::TimeVal;
use nix::unistd::geteuid;

use crate::protocol::MAX_MESSAGE_BYTES;

/// socket の既定パス。`$XDG_RUNTIME_DIR` が無い環境では認証ヘルパを起動できない。
pub fn default_socket_path() -> Result<PathBuf, SocketError> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(SocketError::NoRuntimeDir)?;
    Ok(runtime.join("nekonolock/auth.sock"))
}

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("socket の入出力に失敗した: {0}")]
    Io(#[from] io::Error),
    #[error("socket API に失敗した: {0}")]
    Nix(#[from] nix::Error),
    #[error("XDG_RUNTIME_DIR が無いか絶対パスでない")]
    NoRuntimeDir,
    #[error("socket の親ディレクトリが安全でない: {0}")]
    UnsafeDirectory(PathBuf),
    #[error("socket のパスが安全でない（所有者違い、または socket ではない）: {0}")]
    UnsafePath(PathBuf),
    #[error("既に socket 以外のファイルがある: {0}")]
    PathInUse(PathBuf),
    #[error("別のプロセスが socket を使用中: {0}")]
    AlreadyRunning(PathBuf),
    #[error("メッセージが上限 {MAX_MESSAGE_BYTES} byte を超えた")]
    Oversized,
    #[error("接続が閉じられた")]
    Closed,
    #[error("読み取りがタイムアウトした")]
    Timeout,
    #[error("ピア uid が一致しない（要求 {expected}, 実際 {actual}）")]
    WrongPeer { expected: u32, actual: u32 },
    #[error("socket が SOCK_SEQPACKET ではない")]
    WrongSocketType,
}

/// listen 中の socket。drop 時に自分で作ったパスを消す。
pub struct Listener {
    fd: OwnedFd,
    bound_path: Option<PathBuf>,
}

impl Listener {
    /// パスを bind して listen する。親ディレクトリが無ければ 0700 で作る。
    pub fn bind(path: &Path) -> Result<Self, SocketError> {
        let parent = path
            .parent()
            .ok_or_else(|| SocketError::UnsafeDirectory(PathBuf::from("親ディレクトリが無い")))?;
        ensure_private_dir(parent)?;

        match fs::symlink_metadata(path) {
            Ok(meta) => {
                if !meta.file_type().is_socket() {
                    return Err(SocketError::PathInUse(path.to_path_buf()));
                }
                if meta.uid() != geteuid().as_raw() {
                    return Err(SocketError::UnsafePath(path.to_path_buf()));
                }
                // 生きている socket なら二重起動。接続できなければ古い socket とみなす。
                let probe = socket(
                    nix::sys::socket::AddressFamily::Unix,
                    SockType::SeqPacket,
                    SockFlag::SOCK_CLOEXEC,
                    None,
                )?;
                let addr = UnixAddr::new(path)?;
                match nix_connect(probe.as_raw_fd(), &addr) {
                    Ok(()) => return Err(SocketError::AlreadyRunning(path.to_path_buf())),
                    Err(nix::Error::ECONNREFUSED | nix::Error::ENOENT) => {
                        // ディレクトリは 0700 で所有者は自分なので、古い socket は消してよい。
                        fs::remove_file(path)?;
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }

        let fd = socket(
            nix::sys::socket::AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )?;
        let addr = UnixAddr::new(path)?;
        bind(fd.as_raw_fd(), &addr)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        listen(&fd, Backlog::new(16)?)?;

        Ok(Self {
            fd,
            bound_path: Some(path.to_path_buf()),
        })
    }

    /// systemd socket activation など、外で作られた fd を引き取る。
    /// パスの後始末は呼び出し側（systemd）に任せる。
    pub fn from_owned_fd(fd: OwnedFd) -> Result<Self, SocketError> {
        // 設計は SOCK_SEQPACKET。systemd unit の ListenStream= などの設定ミスを
        // そのまま受け入れると、1 メッセージ 1 要求の前提が崩れる。
        if getsockopt(&fd, sockopt::SockType)? != SockType::SeqPacket {
            return Err(SocketError::WrongSocketType);
        }
        Ok(Self {
            fd,
            bound_path: None,
        })
    }

    /// 同じ uid の接続だけを受け付ける。他人の uid は黙って閉じる。
    pub fn accept_allowed(&self) -> Result<Connection, SocketError> {
        loop {
            let fd = match accept4(&self.fd, SockFlag::SOCK_CLOEXEC) {
                Ok(fd) => fd,
                Err(nix::Error::EINTR) => continue,
                Err(err) => return Err(err.into()),
            };

            let credential = getsockopt(&fd, sockopt::PeerCredentials)?;
            let expected = geteuid().as_raw();
            if credential.uid() != expected {
                eprintln!(
                    "nekonolock-auth: 別 uid ({}) の接続を拒否した",
                    credential.uid()
                );
                drop(fd);
                continue;
            }
            return Ok(Connection { fd });
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(path) = self.bound_path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// accept 済みの 1 接続。
pub struct Connection {
    fd: OwnedFd,
}

/// 同じ uid が listen している socket へ接続する。
///
/// パスは socket で、所有者が実効 uid と一致することを確認する。
/// 接続後は `SO_PEERCRED` でも相手の uid を確認する。
pub fn connect(path: &Path) -> Result<Connection, SocketError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == geteuid().as_raw() => {}
        Ok(_) => return Err(SocketError::UnsafePath(path.to_path_buf())),
        Err(err) => return Err(err.into()),
    }

    let fd = socket(
        nix::sys::socket::AddressFamily::Unix,
        SockType::SeqPacket,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    let addr = UnixAddr::new(path)?;
    nix_connect(fd.as_raw_fd(), &addr)?;

    let credential = getsockopt(&fd, sockopt::PeerCredentials)?;
    let expected = geteuid().as_raw();
    if credential.uid() != expected {
        return Err(SocketError::WrongPeer {
            expected,
            actual: credential.uid(),
        });
    }

    Ok(Connection { fd })
}

impl Connection {
    pub fn peer_uid(&self) -> Result<u32, SocketError> {
        Ok(getsockopt(&self.fd, sockopt::PeerCredentials)?.uid())
    }

    /// 1 メッセージを読む。タイムアウトは `SO_RCVTIMEO` で付ける。
    pub fn read_message(&self, timeout: Duration) -> Result<Vec<u8>, SocketError> {
        let seconds = timeout.as_secs().min(i64::MAX as u64) as i64;
        let micros = timeout.subsec_micros() as i64;
        setsockopt(
            &self.fd,
            sockopt::ReceiveTimeout,
            &TimeVal::new(seconds, micros),
        )?;

        let mut buf = vec![0u8; MAX_MESSAGE_BYTES + 1];
        let len = loop {
            match recv(self.fd.as_raw_fd(), &mut buf, MsgFlags::empty()) {
                Ok(0) => return Err(SocketError::Closed),
                Ok(len) => break len,
                Err(nix::Error::EAGAIN) => return Err(SocketError::Timeout),
                Err(nix::Error::EINTR) => continue,
                Err(err) => return Err(err.into()),
            }
        };
        if len > MAX_MESSAGE_BYTES {
            return Err(SocketError::Oversized);
        }
        buf.truncate(len);
        Ok(buf)
    }

    pub fn write_message(&self, bytes: &[u8]) -> Result<(), SocketError> {
        let sent = send(self.fd.as_raw_fd(), bytes, MsgFlags::empty())?;
        if sent != bytes.len() {
            return Err(SocketError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "メッセージを最後まで送れなかった",
            )));
        }
        Ok(())
    }
}

/// 0700 のディレクトリを用意する。既にある場合は所有者とモードを確認する。
fn ensure_private_dir(path: &Path) -> Result<(), SocketError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_dir()
                || meta.uid() != geteuid().as_raw()
                || meta.permissions().mode() & 0o077 != 0
            {
                return Err(SocketError::UnsafeDirectory(path.to_path_buf()));
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("fd", &self.fd.as_raw_fd())
            .field("bound_path", &self.bound_path)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn unique_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nekonolock-socket-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn bind_sets_private_permissions() {
        let dir = unique_dir();
        let path = dir.join("auth.sock");
        let listener = Listener::bind(&path).unwrap();

        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let sock_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(sock_mode, 0o600);

        drop(listener);
        assert!(!path.exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn from_owned_fd_rejects_stream_socket() {
        let fd = socket(
            nix::sys::socket::AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .unwrap();
        assert!(matches!(
            Listener::from_owned_fd(fd),
            Err(SocketError::WrongSocketType)
        ));
    }

    #[test]
    fn refuses_second_listener_on_same_path() {
        let dir = unique_dir();
        let path = dir.join("auth.sock");
        let listener = Listener::bind(&path).unwrap();
        assert!(matches!(
            Listener::bind(&path),
            Err(SocketError::AlreadyRunning(_))
        ));
        drop(listener);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_directory_with_group_access() {
        let dir = unique_dir();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("auth.sock");
        assert!(matches!(
            Listener::bind(&path),
            Err(SocketError::UnsafeDirectory(_))
        ));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn accepts_same_uid_and_times_out_without_message() {
        let dir = unique_dir();
        let path = dir.join("auth.sock");
        let listener = Listener::bind(&path).unwrap();

        // SOCK_SEQPACKET で接続する。std の UnixStream は SOCK_STREAM なので使わない。
        let client = nix::sys::socket::socket(
            nix::sys::socket::AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .unwrap();
        let addr = UnixAddr::new(&path).unwrap();
        nix::sys::socket::connect(client.as_raw_fd(), &addr).unwrap();

        let conn = listener.accept_allowed().unwrap();
        assert_eq!(conn.peer_uid().unwrap(), geteuid().as_raw());
        assert!(matches!(
            conn.read_message(Duration::from_millis(50)),
            Err(SocketError::Timeout)
        ));

        // そのままでは SOCK_STREAM の相互接続はできないことの確認（型の取り違え防止）。
        assert!(UnixStream::connect(&path).is_err());

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reads_and_writes_one_message() {
        let dir = unique_dir();
        let path = dir.join("auth.sock");
        let listener = Listener::bind(&path).unwrap();

        let client = nix::sys::socket::socket(
            nix::sys::socket::AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .unwrap();
        let addr = UnixAddr::new(&path).unwrap();
        nix::sys::socket::connect(client.as_raw_fd(), &addr).unwrap();

        let conn = listener.accept_allowed().unwrap();
        nix::sys::socket::send(client.as_raw_fd(), b"hello", MsgFlags::empty()).unwrap();
        assert_eq!(conn.read_message(Duration::from_secs(1)).unwrap(), b"hello");
        conn.write_message(b"world").unwrap();

        let mut buf = [0u8; 16];
        let len = recv(client.as_raw_fd(), &mut buf, MsgFlags::empty()).unwrap();
        assert_eq!(&buf[..len], b"world");

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn client_connect_checks_owner_and_exchanges_message() {
        let dir = unique_dir();
        let path = dir.join("auth.sock");
        let listener = Listener::bind(&path).unwrap();

        let server = std::thread::spawn(move || {
            let conn = listener.accept_allowed().unwrap();
            let request = conn.read_message(Duration::from_secs(1)).unwrap();
            assert_eq!(request, b"ping");
            conn.write_message(b"pong").unwrap();
        });

        let conn = connect(&path).unwrap();
        conn.write_message(b"ping").unwrap();
        assert_eq!(conn.read_message(Duration::from_secs(1)).unwrap(), b"pong");
        server.join().unwrap();

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn client_connect_rejects_non_socket_path() {
        let dir = unique_dir();
        let path = dir.join("config.toml");
        fs::write(&path, b"x").unwrap();
        assert!(matches!(connect(&path), Err(SocketError::UnsafePath(_))));
        fs::remove_dir_all(dir).ok();
    }
}
