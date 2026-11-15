//! 認証ヘルパとの 1 要求 1 応答。
//!
//! UI は認証ロジックを持たない。ここでは socket へ要求を送り、応答を
//! [`Outcome`] に写すだけにする。秘密を含む送信バッファは使用後に zeroize する。

use std::path::Path;
use std::time::Duration;

use nekonolock_proto::{
    AuthKind, AuthMethod, PROTOCOL_VERSION, Reason, Request, Response, ResponseBody, connect,
};
use zeroize::Zeroizing;

/// ヘルパから返った結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Status {
        card: bool,
        methods: Vec<AuthKind>,
    },
    Unlocked(AuthMethod),
    Failed(Reason),
    /// 接続できない、応答が読めない、想定外の応答など。ロックは維持する。
    Unavailable(String),
}

/// `status` を問い合わせる。秘密は送らない。
pub fn status(socket: &Path, timeout: Duration) -> Outcome {
    let response = match round_trip(socket, &Request::status(), timeout) {
        Ok(response) => response,
        Err(err) => return Outcome::Unavailable(err),
    };
    match response.body {
        ResponseBody::Status {
            ok: true,
            card,
            methods,
        } => Outcome::Status { card, methods },
        ResponseBody::Status { ok: false, .. } => {
            Outcome::Unavailable("status の応答が ok=false".to_string())
        }
        ResponseBody::Error { reason, .. } => Outcome::Failed(reason),
        ResponseBody::Unlock { .. } => {
            Outcome::Unavailable("status 要求に unlock の応答が返った".to_string())
        }
    }
}

/// `unlock` を送る。`secret` は要求の所有権に移し、drop 時に zeroize される。
pub fn unlock(
    socket: &Path,
    kind: AuthKind,
    secret: impl Into<String>,
    timeout: Duration,
) -> Outcome {
    let request = Request::unlock(kind, secret);
    let response = match round_trip(socket, &request, timeout) {
        Ok(response) => response,
        Err(err) => return Outcome::Unavailable(err),
    };
    match response.body {
        ResponseBody::Unlock { ok: true, method } => Outcome::Unlocked(method),
        ResponseBody::Unlock { ok: false, .. } => {
            Outcome::Unavailable("unlock の応答が不正".to_string())
        }
        ResponseBody::Error { reason, .. } => Outcome::Failed(reason),
        ResponseBody::Status { .. } => {
            Outcome::Unavailable("unlock 要求に status の応答が返った".to_string())
        }
    }
}

fn round_trip(socket: &Path, request: &Request, timeout: Duration) -> Result<Response, String> {
    let bytes = Zeroizing::new(
        serde_json::to_vec(request).map_err(|err| format!("要求を JSON にできない: {err}"))?,
    );
    let connection = connect(socket).map_err(|err| format!("接続できない: {err}"))?;
    connection
        .write_message(&bytes)
        .map_err(|err| format!("要求を送れない: {err}"))?;
    let response = connection
        .read_message(timeout)
        .map_err(|err| format!("応答を読めない: {err}"))?;

    let response: Response = serde_json::from_slice(&response)
        .map_err(|err| format!("応答を JSON として解釈できない: {err}"))?;
    if response.v != PROTOCOL_VERSION {
        return Err(format!("応答のプロトコルバージョンが違う: {}", response.v));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::thread;

    use nekonolock_proto::{Listener, RequestBody};

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nekonolock-auth-client-test-{}-{}",
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

    fn serve_once<F>(dir: PathBuf, responder: F) -> (PathBuf, thread::JoinHandle<()>)
    where
        F: FnOnce(&mut nekonolock_proto::Connection) + Send + 'static,
    {
        let path = dir.join("auth.sock");
        let listener = Listener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            let mut connection = listener.accept_allowed().unwrap();
            responder(&mut connection);
        });
        (path, handle)
    }

    #[test]
    fn status_maps_card_and_methods() {
        let dir = temp_dir();
        let (path, server) = serve_once(dir.clone(), |connection| {
            let request = connection.read_message(Duration::from_secs(1)).unwrap();
            let request: Request = serde_json::from_slice(&request).unwrap();
            assert_eq!(request.body, RequestBody::Status);
            let response = Response::status(false, [AuthKind::Password]);
            connection
                .write_message(&serde_json::to_vec(&response).unwrap())
                .unwrap();
        });

        assert_eq!(
            status(&path, Duration::from_secs(1)),
            Outcome::Status {
                card: false,
                methods: vec![AuthKind::Password],
            }
        );
        server.join().unwrap();
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unlock_sends_kind_and_secret_and_maps_success() {
        let dir = temp_dir();
        let (path, server) = serve_once(dir.clone(), |connection| {
            let request = connection.read_message(Duration::from_secs(1)).unwrap();
            let request: Request = serde_json::from_slice(&request).unwrap();
            match request.body {
                RequestBody::Unlock { kind, secret } => {
                    assert_eq!(kind, AuthKind::Password);
                    assert_eq!(secret.expose(), "test-password");
                }
                RequestBody::Status => panic!("unlock 要求が来ていない"),
            }
            let response = Response::unlocked(AuthMethod::Password);
            connection
                .write_message(&serde_json::to_vec(&response).unwrap())
                .unwrap();
        });

        assert_eq!(
            unlock(
                &path,
                AuthKind::Password,
                "test-password",
                Duration::from_secs(1)
            ),
            Outcome::Unlocked(AuthMethod::Password)
        );
        server.join().unwrap();
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn auth_failure_is_mapped() {
        let dir = temp_dir();
        let (path, server) = serve_once(dir.clone(), |connection| {
            let _ = connection.read_message(Duration::from_secs(1)).unwrap();
            let response = Response::failed(Reason::AuthFailed);
            connection
                .write_message(&serde_json::to_vec(&response).unwrap())
                .unwrap();
        });

        assert_eq!(
            unlock(&path, AuthKind::Pin, "000000", Duration::from_secs(1)),
            Outcome::Failed(Reason::AuthFailed)
        );
        server.join().unwrap();
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_socket_is_unavailable() {
        let dir = temp_dir();
        let path = dir.join("missing.sock");
        assert!(matches!(
            status(&path, Duration::from_secs(1)),
            Outcome::Unavailable(_)
        ));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn wrong_protocol_version_is_unavailable() {
        let dir = temp_dir();
        let (path, server) = serve_once(dir.clone(), |connection| {
            let _ = connection.read_message(Duration::from_secs(1)).unwrap();
            connection
                .write_message(br#"{"v":2,"ok":true,"card":true,"methods":[]}"#)
                .unwrap();
        });

        assert!(matches!(
            status(&path, Duration::from_secs(1)),
            Outcome::Unavailable(_)
        ));
        server.join().unwrap();
        fs::remove_dir_all(dir).ok();
    }
}
