//! 要求の受理・拒否の判断。UI から渡された `kind` をそのまま尊重する。
//!
//! パスワードを PIN として試すことは絶対にしない。モードの取り違えは
//! カードのリトライ回数を消費する事故になるため、ここでは `kind` を
//! 設定と照合するだけで、別方式へのフォールバックは行わない。

use nekonolock_proto::{
    AuthConfig, AuthKind, AuthMethod, Reason, Request, RequestBody, Response, Secret,
};

use crate::pam::{PamError, PasswordAuthenticator};
use nekonolock_gpg::{CardError, CardSigner};

pub struct Engine<C, P> {
    config: AuthConfig,
    user: String,
    card: C,
    pam: P,
}

impl<C: CardSigner, P: PasswordAuthenticator> Engine<C, P> {
    pub fn new(config: AuthConfig, user: impl Into<String>, card: C, pam: P) -> Self {
        Self {
            config,
            user: user.into(),
            card,
            pam,
        }
    }

    pub fn handle(&self, request: &Request) -> Response {
        if !request.validate() {
            return Response::failed(Reason::BadRequest);
        }
        match &request.body {
            RequestBody::Status => self.status(),
            RequestBody::Unlock { kind, secret } => self.unlock(*kind, secret),
        }
    }

    fn status(&self) -> Response {
        let mut methods = Vec::new();
        if self.config.openpgp {
            methods.push(AuthKind::Pin);
        }
        if self.config.password {
            methods.push(AuthKind::Password);
        }
        Response::status(self.card.present(), methods)
    }

    fn unlock(&self, kind: AuthKind, secret: &Secret) -> Response {
        if secret.is_empty() {
            return Response::failed(Reason::BadRequest);
        }
        match kind {
            AuthKind::Pin => self.unlock_pin(secret),
            AuthKind::Password => self.unlock_password(secret),
        }
    }

    fn unlock_pin(&self, secret: &Secret) -> Response {
        if !self.config.openpgp {
            return Response::failed(Reason::PinDisabled);
        }
        if !self.card.present() {
            return Response::failed(Reason::NoCard);
        }

        let mut challenge = vec![0u8; self.config.challenge_bytes];
        if getrandom::fill(&mut challenge).is_err() {
            return Response::failed(Reason::GpgError);
        }

        let signature = match self.card.sign(&challenge, secret.expose()) {
            Ok(signature) => signature,
            Err(CardError::BadPin) => return Response::failed(Reason::AuthFailed),
            Err(CardError::Timeout | CardError::Unavailable(_) | CardError::Io(_)) => {
                return Response::failed(Reason::GpgError);
            }
        };

        match self.card.verify(&challenge, &signature) {
            Ok(true) => Response::unlocked(AuthMethod::Openpgp),
            Ok(false) => Response::failed(Reason::AuthFailed),
            Err(CardError::BadPin) => Response::failed(Reason::AuthFailed),
            Err(_) => Response::failed(Reason::GpgError),
        }
    }

    fn unlock_password(&self, secret: &Secret) -> Response {
        if !self.config.password {
            return Response::failed(Reason::PasswordDisabled);
        }

        match self.pam.authenticate(&self.user, secret.expose()) {
            Ok(true) => Response::unlocked(AuthMethod::Password),
            Ok(false) => Response::failed(Reason::AuthFailed),
            Err(PamError::Load(_) | PamError::Call(_)) => Response::failed(Reason::PamError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestCard {
        present: bool,
        pin: &'static str,
        tamper: bool,
    }

    impl TestCard {
        fn new() -> Self {
            Self {
                present: true,
                pin: "123456",
                tamper: false,
            }
        }
    }

    impl CardSigner for TestCard {
        fn present(&self) -> bool {
            self.present
        }

        fn sign(&self, challenge: &[u8], pin: &str) -> Result<Vec<u8>, CardError> {
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
            Ok(challenge == signature)
        }
    }

    enum PamMode {
        Accept,
        Reject,
        Error,
    }

    struct TestPam(PamMode);

    impl PasswordAuthenticator for TestPam {
        fn authenticate(&self, _user: &str, _password: &str) -> Result<bool, PamError> {
            match self.0 {
                PamMode::Accept => Ok(true),
                PamMode::Reject => Ok(false),
                PamMode::Error => Err(PamError::Call("会話に失敗した".to_string())),
            }
        }
    }

    fn engine(config: AuthConfig, card: TestCard, pam: PamMode) -> Engine<TestCard, TestPam> {
        Engine::new(config, "tester", card, TestPam(pam))
    }

    fn pin_attempt() -> Request {
        Request::unlock(AuthKind::Pin, "123456")
    }

    fn password_attempt() -> Request {
        Request::unlock(AuthKind::Password, "test-password")
    }

    #[test]
    fn status_lists_enabled_methods_and_card() {
        let config = AuthConfig::default();
        let response =
            engine(config.clone(), TestCard::new(), PamMode::Accept).handle(&Request::status());
        assert_eq!(
            response,
            Response::status(true, [AuthKind::Pin, AuthKind::Password])
        );

        let config = AuthConfig {
            openpgp: false,
            ..config
        };
        let response = engine(config, TestCard::new(), PamMode::Accept).handle(&Request::status());
        assert_eq!(response, Response::status(true, [AuthKind::Password]));
    }

    #[test]
    fn pin_success_uses_openpgp() {
        let response =
            engine(AuthConfig::default(), TestCard::new(), PamMode::Reject).handle(&pin_attempt());
        assert_eq!(response, Response::unlocked(AuthMethod::Openpgp));
    }

    #[test]
    fn pin_failures_stay_locked() {
        let mut card = TestCard::new();
        card.pin = "000000";
        assert_eq!(
            engine(AuthConfig::default(), card, PamMode::Reject)
                .handle(&pin_attempt())
                .reason(),
            Some(Reason::AuthFailed)
        );

        let mut card = TestCard::new();
        card.present = false;
        assert_eq!(
            engine(AuthConfig::default(), card, PamMode::Reject)
                .handle(&pin_attempt())
                .reason(),
            Some(Reason::NoCard)
        );

        let mut card = TestCard::new();
        card.tamper = true;
        assert_eq!(
            engine(AuthConfig::default(), card, PamMode::Reject)
                .handle(&pin_attempt())
                .reason(),
            Some(Reason::AuthFailed)
        );
    }

    #[test]
    fn disabled_methods_are_rejected_explicitly() {
        let config = AuthConfig {
            openpgp: false,
            ..AuthConfig::default()
        };
        assert_eq!(
            engine(config, TestCard::new(), PamMode::Accept)
                .handle(&pin_attempt())
                .reason(),
            Some(Reason::PinDisabled)
        );

        let config = AuthConfig {
            password: false,
            ..AuthConfig::default()
        };
        assert_eq!(
            engine(config, TestCard::new(), PamMode::Accept)
                .handle(&password_attempt())
                .reason(),
            Some(Reason::PasswordDisabled)
        );
    }

    #[test]
    fn password_results_are_mapped() {
        assert_eq!(
            engine(AuthConfig::default(), TestCard::new(), PamMode::Accept)
                .handle(&password_attempt()),
            Response::unlocked(AuthMethod::Password)
        );
        assert_eq!(
            engine(AuthConfig::default(), TestCard::new(), PamMode::Reject)
                .handle(&password_attempt())
                .reason(),
            Some(Reason::AuthFailed)
        );
        assert_eq!(
            engine(AuthConfig::default(), TestCard::new(), PamMode::Error)
                .handle(&password_attempt())
                .reason(),
            Some(Reason::PamError)
        );
    }

    #[test]
    fn bad_requests_are_rejected() {
        let engine = engine(AuthConfig::default(), TestCard::new(), PamMode::Accept);

        let mut request = pin_attempt();
        request.v = 99;
        assert_eq!(engine.handle(&request).reason(), Some(Reason::BadRequest));

        let empty = Request::unlock(AuthKind::Pin, "");
        assert_eq!(engine.handle(&empty).reason(), Some(Reason::BadRequest));
    }
}
