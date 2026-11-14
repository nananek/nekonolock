//! テスト専用のモック。`mock-card` / `mock-pam` feature を付けたデバッグ
//! ビルドだけで使う。配布ビルドでは main.rs が release 時に拒否する。

use std::env;

use crate::card::{CardError, CardSigner};
use crate::pam::{PamError, PasswordAuthenticator};

pub const MOCK_PIN: &str = "123456";
pub const MOCK_PASSWORD: &str = "test-password";

pub struct MockCard {
    present: bool,
    failing: bool,
}

impl MockCard {
    pub fn new() -> Self {
        let mode = env::var("NEKONOLOCK_MOCK_CARD").unwrap_or_default();
        Self {
            present: mode != "absent",
            failing: mode == "error",
        }
    }
}

impl CardSigner for MockCard {
    fn present(&self) -> bool {
        self.present
    }

    fn sign(&self, challenge: &[u8], pin: &str) -> Result<Vec<u8>, CardError> {
        if self.failing {
            return Err(CardError::Unavailable("mock card error".to_string()));
        }
        if pin != MOCK_PIN {
            return Err(CardError::BadPin);
        }
        Ok(challenge.to_vec())
    }

    fn verify(&self, challenge: &[u8], signature: &[u8]) -> Result<bool, CardError> {
        if self.failing {
            return Err(CardError::Unavailable("mock card error".to_string()));
        }
        Ok(challenge == signature)
    }
}

pub struct MockPam {
    failing: bool,
}

impl MockPam {
    pub fn new() -> Self {
        Self {
            failing: env::var("NEKONOLOCK_MOCK_PAM").as_deref() == Ok("error"),
        }
    }
}

impl PasswordAuthenticator for MockPam {
    fn authenticate(&self, _user: &str, password: &str) -> Result<bool, PamError> {
        if self.failing {
            return Err(PamError::Call("会話に失敗した".to_string()));
        }
        Ok(password == MOCK_PASSWORD)
    }
}
