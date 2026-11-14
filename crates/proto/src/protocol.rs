//! UI と認証ヘルパの間でやりとりする 1 オブジェクトの JSON プロトコル。
//!
//! 1 接続 1 要求。要求と応答はそれぞれ 1 個の JSON オブジェクトで、
//! `SOCK_SEQPACKET` の 1 メッセージに収める。

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// プロトコルのバージョン。互換性を壊す変更のときだけ上げる。
pub const PROTOCOL_VERSION: u32 = 1;

/// 1 メッセージの上限。ソケットから読むときにこの長さを超えたら拒否する。
pub const MAX_MESSAGE_BYTES: usize = 4096;

/// 秘密（PIN / パスワード）の上限。長すぎる入力は `bad-request` にする。
pub const MAX_SECRET_BYTES: usize = 512;

/// 解除方式。UI が明示的に選び、ヘルパはこの値をそのまま尊重する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthKind {
    Pin,
    Password,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Password => "password",
        }
    }
}

/// 実際に成功した認証方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Openpgp,
    Password,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openpgp => "openpgp",
            Self::Password => "password",
        }
    }
}

/// 認証に失敗した理由。秘密に関する情報（PIN の残回数など）は含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    AuthFailed,
    NoCard,
    GpgError,
    PamError,
    PinDisabled,
    PasswordDisabled,
    BadRequest,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthFailed => "auth-failed",
            Self::NoCard => "no-card",
            Self::GpgError => "gpg-error",
            Self::PamError => "pam-error",
            Self::PinDisabled => "pin-disabled",
            Self::PasswordDisabled => "password-disabled",
            Self::BadRequest => "bad-request",
        }
    }
}

/// ログや `Debug` に出さない秘密。
///
/// 所有権を持ち、drop 時にバッファを zeroize する。比較はテスト用で、
/// タイミング安全ではない（認証判定には使わない）。
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// UI からヘルパへの要求。
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    pub v: u32,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum RequestBody {
    Status,
    Unlock { kind: AuthKind, secret: Secret },
}

impl Request {
    pub fn status() -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: RequestBody::Status,
        }
    }

    pub fn unlock(kind: AuthKind, secret: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: RequestBody::Unlock {
                kind,
                secret: Secret::new(secret),
            },
        }
    }

    /// バージョンと秘密の長さを検証する。
    pub fn validate(&self) -> bool {
        if self.v != PROTOCOL_VERSION {
            return false;
        }
        match &self.body {
            RequestBody::Status => true,
            RequestBody::Unlock { secret, .. } => {
                !secret.is_empty() && secret.len() <= MAX_SECRET_BYTES
            }
        }
    }
}

/// ヘルパから UI への応答。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    pub v: u32,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ResponseBody {
    Status {
        ok: bool,
        card: bool,
        methods: Vec<AuthKind>,
    },
    Unlock {
        ok: bool,
        method: AuthMethod,
    },
    Error {
        ok: bool,
        reason: Reason,
    },
}

impl Response {
    pub fn status(card: bool, methods: impl IntoIterator<Item = AuthKind>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: ResponseBody::Status {
                ok: true,
                card,
                methods: methods.into_iter().collect(),
            },
        }
    }

    pub fn unlocked(method: AuthMethod) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: ResponseBody::Unlock { ok: true, method },
        }
    }

    pub fn failed(reason: Reason) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            body: ResponseBody::Error { ok: false, reason },
        }
    }

    pub fn is_ok(&self) -> bool {
        match self.body {
            ResponseBody::Status { ok, .. }
            | ResponseBody::Unlock { ok, .. }
            | ResponseBody::Error { ok, .. } => ok,
        }
    }

    pub fn reason(&self) -> Option<Reason> {
        match self.body {
            ResponseBody::Error { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_request(json: &str) -> Request {
        serde_json::from_str(json).expect("要求を解釈できる")
    }

    fn parse_response(json: &str) -> Response {
        serde_json::from_str(json).expect("応答を解釈できる")
    }

    #[test]
    fn status_request_round_trip() {
        let request = Request::status();
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"v":1,"op":"status"}"#
        );
        assert_eq!(parse_request(r#"{"v":1,"op":"status"}"#), request);
    }

    #[test]
    fn unlock_request_round_trip() {
        let request = Request::unlock(AuthKind::Pin, "123456");
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"v":1,"op":"unlock","kind":"pin","secret":"123456"}"#
        );
        assert_eq!(
            parse_request(r#"{"v":1,"op":"unlock","kind":"pin","secret":"123456"}"#),
            request
        );

        let request = Request::unlock(AuthKind::Password, "correct horse");
        assert_eq!(
            parse_request(&serde_json::to_string(&request).unwrap()),
            request
        );
    }

    #[test]
    fn status_response_shape() {
        let response = Response::status(true, [AuthKind::Pin, AuthKind::Password]);
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"v":1,"ok":true,"card":true,"methods":["pin","password"]}"#
        );
        assert_eq!(
            parse_response(r#"{"v":1,"ok":true,"card":false,"methods":["password"]}"#),
            Response::status(false, [AuthKind::Password])
        );
    }

    #[test]
    fn unlock_response_shape() {
        let response = Response::unlocked(AuthMethod::Openpgp);
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"v":1,"ok":true,"method":"openpgp"}"#
        );
        assert_eq!(
            parse_response(r#"{"v":1,"ok":true,"method":"password"}"#),
            Response::unlocked(AuthMethod::Password)
        );
    }

    #[test]
    fn error_response_shape() {
        let response = Response::failed(Reason::AuthFailed);
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"v":1,"ok":false,"reason":"auth-failed"}"#
        );
        assert_eq!(
            parse_response(r#"{"v":1,"ok":false,"reason":"password-disabled"}"#),
            Response::failed(Reason::PasswordDisabled)
        );
        assert!(!response.is_ok());
        assert_eq!(response.reason(), Some(Reason::AuthFailed));
    }

    #[test]
    fn every_reason_is_kebab_case() {
        let reasons = [
            (Reason::AuthFailed, "auth-failed"),
            (Reason::NoCard, "no-card"),
            (Reason::GpgError, "gpg-error"),
            (Reason::PamError, "pam-error"),
            (Reason::PinDisabled, "pin-disabled"),
            (Reason::PasswordDisabled, "password-disabled"),
            (Reason::BadRequest, "bad-request"),
        ];
        for (reason, text) in reasons {
            assert_eq!(
                serde_json::to_string(&reason).unwrap(),
                format!("\"{text}\"")
            );
        }
    }

    #[test]
    fn validate_rejects_wrong_version_and_empty_secret() {
        let mut request = Request::status();
        request.v = 2;
        assert!(!request.validate());

        assert!(!Request::unlock(AuthKind::Pin, "").validate());
        assert!(!Request::unlock(AuthKind::Pin, "x".repeat(MAX_SECRET_BYTES + 1)).validate());
        assert!(Request::unlock(AuthKind::Pin, "123456").validate());
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new("very-secret");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert!(!format!("{secret:?}").contains("very-secret"));
    }
}
