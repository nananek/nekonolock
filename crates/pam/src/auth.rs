//! PIN を受け取って、カードのチャレンジ署名を検証する。
//!
//! 署名はユーザーの gpg-agent / scdaemon にさせるが、検証は root 側の
//! 信頼した鍵で行う（`crate::module` が 2 つの `GpgCard` を組み合わせる）。

use nekonolock_gpg::{CardError, CardSigner};

/// 認証の結果。PAM の戻り値への変換は呼び出し側で行う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Unlocked,
    WrongPin,
    Error(String),
}

pub fn authenticate(card: &impl CardSigner, challenge_bytes: usize, pin: &str) -> Outcome {
    if pin.is_empty() {
        return Outcome::WrongPin;
    }

    let mut challenge = vec![0u8; challenge_bytes];
    if getrandom::fill(&mut challenge).is_err() {
        return Outcome::Error("乱数を生成できない".to_string());
    }

    let signature = match card.sign(&challenge, pin) {
        Ok(signature) => signature,
        Err(CardError::BadPin) => return Outcome::WrongPin,
        Err(CardError::Timeout) => return Outcome::Error("カードがタイムアウトした".to_string()),
        Err(CardError::Unavailable(message)) => return Outcome::Error(message),
        Err(CardError::Io(err)) => return Outcome::Error(err.to_string()),
    };

    match card.verify(&challenge, &signature) {
        Ok(true) => Outcome::Unlocked,
        Ok(false) => Outcome::WrongPin,
        Err(CardError::BadPin) => Outcome::WrongPin,
        Err(CardError::Timeout) => Outcome::Error("検証がタイムアウトした".to_string()),
        Err(CardError::Unavailable(message)) => Outcome::Error(message),
        Err(CardError::Io(err)) => Outcome::Error(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestCard {
        present: bool,
        pin: &'static str,
        tamper: bool,
        error: bool,
    }

    impl TestCard {
        fn new() -> Self {
            Self {
                present: true,
                pin: "123456",
                tamper: false,
                error: false,
            }
        }
    }

    impl CardSigner for TestCard {
        fn present(&self) -> bool {
            self.present
        }

        fn sign(&self, challenge: &[u8], pin: &str) -> Result<Vec<u8>, CardError> {
            if self.error {
                return Err(CardError::Unavailable("mock".to_string()));
            }
            if pin != self.pin {
                return Err(CardError::BadPin);
            }
            let mut signature = challenge.to_vec();
            if self.tamper {
                signature[0] ^= 0xff;
            }
            Ok(signature)
        }

        fn verify(&self, challenge: &[u8], signature: &[u8]) -> Result<bool, CardError> {
            if self.error {
                return Err(CardError::Unavailable("mock".to_string()));
            }
            Ok(challenge == signature)
        }
    }

    #[test]
    fn accepts_the_right_pin() {
        assert_eq!(
            authenticate(&TestCard::new(), 32, "123456"),
            Outcome::Unlocked
        );
    }

    #[test]
    fn rejects_wrong_pin_and_bad_signature() {
        let mut card = TestCard::new();
        card.pin = "000000";
        assert_eq!(authenticate(&card, 32, "123456"), Outcome::WrongPin);

        let mut card = TestCard::new();
        card.tamper = true;
        assert_eq!(authenticate(&card, 32, "123456"), Outcome::WrongPin);

        assert_eq!(authenticate(&TestCard::new(), 32, ""), Outcome::WrongPin);
    }

    #[test]
    fn reports_card_errors() {
        let mut card = TestCard::new();
        card.error = true;
        assert!(matches!(
            authenticate(&card, 32, "123456"),
            Outcome::Error(_)
        ));
    }
}
