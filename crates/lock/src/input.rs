//! ロック画面の入力状態。
//!
//! - モード（`pin` / `password`）は UI が明示的に持つ。切替時は入力中の文字列を
//!   破棄し、パスワードを PIN として送る事故を防ぐ。
//! - 秘密は [`zeroize::Zeroizing`] で保持し、破棄時にメモリを消す。
//! - 認証は 1 回に 1 つだけ送る。送信中は入力もモード切替も受け付けない。

use std::time::{Duration, Instant};

use nekonolock_proto::{AuthConfig, AuthKind, MAX_SECRET_BYTES, Reason, UiConfig};
use zeroize::{Zeroize, Zeroizing};

/// 入力中のモード。`AuthKind` と同じ 2 種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Pin,
    Password,
}

impl Mode {
    pub fn kind(self) -> AuthKind {
        match self {
            Self::Pin => AuthKind::Pin,
            Self::Password => AuthKind::Password,
        }
    }

    /// モード capsule に出す名前。画面のコピーはすべて大文字に揃える。
    pub fn label(self) -> &'static str {
        match self {
            Self::Pin => "CARD PIN",
            Self::Password => "PASSWORD",
        }
    }

    /// 入力欄の下に出す既定の案内。状態メッセージが無いときだけ使う。
    pub fn hint(self) -> &'static str {
        match self {
            Self::Pin => "TOUCH THE KEY, THEN ENTER YOUR PIN",
            Self::Password => "ENTER YOUR PASSWORD",
        }
    }
}

/// 設定またはヘルパの status から分かる、使える方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Methods {
    pub pin: bool,
    pub password: bool,
}

impl Methods {
    pub fn from_config(config: &AuthConfig) -> Self {
        Self {
            pin: config.openpgp,
            password: config.password,
        }
    }

    pub fn from_status(kinds: &[AuthKind]) -> Self {
        Self {
            pin: kinds.contains(&AuthKind::Pin),
            password: kinds.contains(&AuthKind::Password),
        }
    }

    pub fn has(self, mode: Mode) -> bool {
        match mode {
            Mode::Pin => self.pin,
            Mode::Password => self.password,
        }
    }

    /// 既定モード。カードがあれば PIN、無ければパスワードを優先する。
    /// 片方しか有効でなければ、その方式を返す。
    pub fn default_mode(self, card: Option<bool>) -> Mode {
        if self.pin && card == Some(true) {
            Mode::Pin
        } else if self.password {
            Mode::Password
        } else {
            Mode::Pin
        }
    }

    fn toggle(self, mode: Mode) -> Mode {
        match (mode, self.pin, self.password) {
            (Mode::Pin, _, true) => Mode::Password,
            (Mode::Password, true, _) => Mode::Pin,
            _ => mode,
        }
    }
}

/// 画面に出す短いメッセージ。長い文は使わない（帯と重なるため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    None,
    Checking,
    Authenticating,
    AuthFailed,
    NoCard,
    GpgError,
    PamError,
    HelperUnavailable,
    BadRequest,
    PinDisabled,
    PasswordDisabled,
    ConfigError,
}

/// 入力欄と認証要求の状態。
pub struct UiState {
    methods: Methods,
    mode: Mode,
    secret: Zeroizing<String>,
    busy: bool,
    message: Message,
    card: Option<bool>,
    attempts: u32,
    max_attempts: u32,
    cooldown_until: Option<Instant>,
}

impl UiState {
    pub fn new(ui: &UiConfig, auth: &AuthConfig) -> Self {
        let methods = Methods::from_config(auth);
        Self {
            methods,
            // カードの有無が分かるまではパスワード側に倒す。PIN はカードが
            // 無いと失敗するだけで、パスワードはカードのリトライを消費しない。
            mode: methods.default_mode(None),
            secret: Zeroizing::new(String::new()),
            busy: false,
            message: Message::None,
            card: None,
            attempts: 0,
            max_attempts: ui.max_attempts,
            cooldown_until: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn methods(&self) -> Methods {
        self.methods
    }

    pub fn card(&self) -> Option<bool> {
        self.card
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn secret_len(&self) -> usize {
        self.secret.chars().count()
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// ヘルパへの status 問い合わせを始める。
    pub fn begin_status(&mut self) {
        self.busy = true;
        self.message = Message::Checking;
        self.clear_secret();
    }

    /// status の結果を反映する。
    ///
    /// 初回はカードの有無で既定モードを選ぶ。以降はカードが抜けたときだけ
    /// 一方向にパスワードへ切り替える（挿し直しても PIN には戻さない）。
    pub fn apply_status(&mut self, card: bool, kinds: &[AuthKind]) {
        let previous = self.card;
        self.card = Some(card);
        self.methods = Methods::from_status(kinds);

        if previous.is_none() {
            let desired = self.methods.default_mode(Some(card));
            if desired != self.mode {
                self.switch_mode(desired);
            }
        } else if previous == Some(true) && !card {
            if self.methods.password {
                self.switch_mode(Mode::Password);
            } else {
                self.clear_secret();
            }
        }
        if !self.methods.has(self.mode) {
            self.switch_mode(self.methods.default_mode(Some(card)));
        }

        self.busy = false;
        if !card {
            // カードが無くてもパスワードで解除できる。案内として残す。
            self.message = Message::NoCard;
        } else if matches!(self.message, Message::Checking | Message::NoCard) {
            self.message = Message::None;
        }
    }

    /// ヘルパに接続できない。ロックは維持し、状態だけ知らせる。
    pub fn helper_unavailable(&mut self) {
        self.busy = false;
        self.message = Message::HelperUnavailable;
        // 送信済みの秘密の写しを残さない。
        self.clear_secret();
    }

    /// 設定を読めない。ロックは既定値で維持する。
    pub fn config_error(&mut self) {
        self.busy = false;
        self.message = Message::ConfigError;
        self.clear_secret();
    }

    /// 認証失敗を反映する。秘密は成功・失敗にかかわらず破棄する。
    pub fn finish_failure(&mut self, reason: Reason, now: Instant) {
        self.busy = false;
        self.clear_secret();

        match reason {
            Reason::AuthFailed => {
                self.attempts = self.attempts.saturating_add(1);
                self.message = Message::AuthFailed;
                self.start_cooldown(now);
            }
            Reason::NoCard => {
                self.message = Message::NoCard;
                if self.methods.password && self.mode == Mode::Pin {
                    self.switch_mode(Mode::Password);
                }
            }
            Reason::GpgError => self.message = Message::GpgError,
            Reason::PamError => self.message = Message::PamError,
            Reason::PinDisabled => {
                self.message = Message::PinDisabled;
                if self.methods.password {
                    self.switch_mode(Mode::Password);
                }
            }
            Reason::PasswordDisabled => {
                self.message = Message::PasswordDisabled;
                if self.methods.pin {
                    self.switch_mode(Mode::Pin);
                }
            }
            Reason::BadRequest => self.message = Message::BadRequest,
        }
    }

    /// `Tab` でモードを切り替える。切り替えたら入力を破棄する。
    pub fn toggle_mode(&mut self) -> bool {
        if self.busy {
            return false;
        }
        let next = self.methods.toggle(self.mode);
        if next == self.mode {
            return false;
        }
        self.switch_mode(next);
        true
    }

    /// 文字を 1 つ追加する。受け付けたら true。
    pub fn push_char(&mut self, c: char) -> bool {
        if self.busy || c.is_control() {
            return false;
        }
        // PIN は数字だけ。パスワードを PIN として送る取り違えを UI 側でも止める。
        if self.mode == Mode::Pin && !c.is_ascii_digit() {
            return false;
        }
        if self.secret.len() + c.len_utf8() > MAX_SECRET_BYTES {
            return false;
        }
        self.secret.push(c);
        true
    }

    pub fn backspace(&mut self) -> bool {
        if self.busy {
            return false;
        }
        self.secret.pop().is_some()
    }

    /// 入力を消す（`Esc`）。
    pub fn clear_secret(&mut self) {
        self.secret.zeroize();
        self.secret.clear();
    }

    /// 送信できるなら要求を作る。送信中・空・クールダウン中は `None`。
    pub fn take_request(&mut self, now: Instant) -> Option<(AuthKind, String)> {
        if self.busy || self.secret.is_empty() || self.in_cooldown(now) {
            return None;
        }
        let kind = self.mode.kind();
        let secret = self.secret.to_string();
        self.busy = true;
        self.message = Message::Authenticating;
        Some((kind, secret))
    }

    fn in_cooldown(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| now < until)
    }

    fn start_cooldown(&mut self, now: Instant) {
        if self.max_attempts == 0 {
            return;
        }
        let delay = if self.attempts >= self.max_attempts {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(1)
        };
        self.cooldown_until = Some(now + delay);
    }

    fn switch_mode(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.clear_secret();
    }

    /// 画面に出す 1 行。秘密は含めない。
    pub fn status_line(&self, now: Instant) -> String {
        if self.busy {
            return match self.message {
                Message::Checking => "CHECKING...".to_string(),
                _ => "AUTHENTICATING...".to_string(),
            };
        }

        if self.max_attempts > 0 && self.attempts >= self.max_attempts && self.in_cooldown(now) {
            let seconds = self
                .cooldown_until
                .map(|until| until.saturating_duration_since(now).as_secs().max(1))
                .unwrap_or(1);
            return format!("WAIT {seconds}s");
        }

        match self.message {
            Message::None => String::new(),
            Message::Checking => "CHECKING...".to_string(),
            Message::Authenticating => "AUTHENTICATING...".to_string(),
            Message::AuthFailed => {
                if self.max_attempts > 0 {
                    format!(
                        "AUTH FAILED ({}/{})",
                        self.attempts.min(self.max_attempts),
                        self.max_attempts
                    )
                } else {
                    "AUTH FAILED".to_string()
                }
            }
            Message::NoCard => "CARD NOT FOUND".to_string(),
            Message::GpgError => "CARD ERROR".to_string(),
            Message::PamError => "PAM ERROR".to_string(),
            Message::HelperUnavailable => "AUTH HELPER UNAVAILABLE".to_string(),
            Message::BadRequest => "BAD REQUEST".to_string(),
            Message::PinDisabled => "PIN DISABLED".to_string(),
            Message::PasswordDisabled => "PASSWORD DISABLED".to_string(),
            Message::ConfigError => "CONFIG ERROR - LOCKED".to_string(),
        }
    }

    #[cfg(test)]
    fn secret(&self) -> &str {
        &self.secret
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> UiState {
        UiState::new(&UiConfig::default(), &AuthConfig::default())
    }

    #[test]
    fn starts_in_password_mode_until_status_arrives() {
        assert_eq!(state().mode(), Mode::Password);
    }

    #[test]
    fn screen_copy_is_uppercase() {
        for mode in [Mode::Pin, Mode::Password] {
            assert_eq!(mode.label(), mode.label().to_uppercase());
            assert_eq!(mode.hint(), mode.hint().to_uppercase());
        }
    }

    #[test]
    fn status_clears_checking_message() {
        let mut ui = state();
        ui.begin_status();
        assert_eq!(ui.status_line(Instant::now()), "CHECKING...");
        ui.apply_status(true, &[AuthKind::Pin, AuthKind::Password]);
        assert_eq!(ui.status_line(Instant::now()), "");
    }

    #[test]
    fn status_selects_pin_when_card_is_present() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Pin, AuthKind::Password]);
        assert_eq!(ui.mode(), Mode::Pin);
        assert_eq!(ui.card(), Some(true));

        let mut ui = state();
        ui.apply_status(false, &[AuthKind::Pin, AuthKind::Password]);
        assert_eq!(ui.mode(), Mode::Password);
    }

    #[test]
    fn status_respects_disabled_methods() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Password]);
        assert_eq!(ui.mode(), Mode::Password);
        assert!(!ui.methods().pin);

        let mut ui = state();
        ui.apply_status(false, &[AuthKind::Pin]);
        assert_eq!(ui.mode(), Mode::Pin);
        assert_eq!(ui.status_line(Instant::now()), "CARD NOT FOUND");
    }

    #[test]
    fn card_removal_switches_to_password_and_clears_input() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Pin, AuthKind::Password]);
        assert_eq!(ui.mode(), Mode::Pin);
        ui.push_char('1');
        assert_eq!(ui.secret(), "1");

        ui.apply_status(false, &[AuthKind::Pin, AuthKind::Password]);
        assert_eq!(ui.mode(), Mode::Password);
        assert_eq!(ui.secret(), "");
        assert_eq!(ui.status_line(Instant::now()), "CARD NOT FOUND");

        // カードを挿し直しても PIN には戻さない。
        ui.apply_status(true, &[AuthKind::Pin, AuthKind::Password]);
        assert_eq!(ui.mode(), Mode::Password);
    }

    #[test]
    fn card_removal_keeps_pin_and_clears_input_when_password_is_disabled() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Pin]);
        ui.push_char('1');

        ui.apply_status(false, &[AuthKind::Pin]);
        assert_eq!(ui.mode(), Mode::Pin);
        assert_eq!(ui.secret(), "");
        assert_eq!(ui.status_line(Instant::now()), "CARD NOT FOUND");
    }

    #[test]
    fn tab_switches_and_clears_input() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Pin, AuthKind::Password]);
        assert_eq!(ui.mode(), Mode::Pin);
        assert!(ui.push_char('1'));
        assert_eq!(ui.secret(), "1");

        assert!(ui.toggle_mode());
        assert_eq!(ui.mode(), Mode::Password);
        assert_eq!(ui.secret(), "");
    }

    #[test]
    fn tab_does_nothing_when_only_one_method_is_enabled() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Pin]);
        assert_eq!(ui.mode(), Mode::Pin);
        assert!(!ui.toggle_mode());
        assert_eq!(ui.mode(), Mode::Pin);
    }

    #[test]
    fn pin_accepts_digits_only_and_password_accepts_text() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Pin, AuthKind::Password]);
        assert!(ui.push_char('1'));
        assert!(!ui.push_char('a'));
        assert!(!ui.push_char(' '));
        assert_eq!(ui.secret(), "1");

        ui.toggle_mode();
        assert!(ui.push_char('a'));
        assert!(ui.push_char(' '));
        assert!(ui.push_char('あ'));
        assert_eq!(ui.secret(), "a あ");
    }

    #[test]
    fn secret_is_limited_to_protocol_max() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Password]);
        for _ in 0..MAX_SECRET_BYTES {
            assert!(ui.push_char('x'));
        }
        assert!(!ui.push_char('y'));
        assert_eq!(ui.secret().len(), MAX_SECRET_BYTES);
    }

    #[test]
    fn backspace_removes_one_character() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Password]);
        ui.push_char('あ');
        ui.push_char('b');
        assert!(ui.backspace());
        assert_eq!(ui.secret(), "あ");
        assert!(ui.backspace());
        assert!(!ui.backspace());
    }

    #[test]
    fn request_is_taken_once_and_cleared_on_failure() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Pin, AuthKind::Password]);
        for c in "123456".chars() {
            ui.push_char(c);
        }
        let now = Instant::now();
        let (kind, secret) = ui.take_request(now).unwrap();
        assert_eq!(kind, AuthKind::Pin);
        assert_eq!(secret, "123456");
        assert!(ui.is_busy());
        assert!(ui.take_request(now).is_none());
        assert_eq!(ui.status_line(now), "AUTHENTICATING...");

        ui.finish_failure(Reason::AuthFailed, now);
        assert!(!ui.is_busy());
        assert_eq!(ui.secret(), "");
        assert_eq!(ui.attempts(), 1);
        assert_eq!(ui.status_line(now), "AUTH FAILED (1/5)");
    }

    #[test]
    fn cooldown_blocks_resubmission_until_it_expires() {
        let now = Instant::now();
        let mut ui = UiState::new(
            &UiConfig {
                max_attempts: 2,
                ..UiConfig::default()
            },
            &AuthConfig::default(),
        );
        ui.apply_status(true, &[AuthKind::Password]);

        for expected in 1..=2 {
            for c in "wrong".chars() {
                ui.push_char(c);
            }
            let attempt_at = now + Duration::from_secs(expected as u64 - 1);
            assert!(ui.take_request(attempt_at).is_some());
            ui.finish_failure(Reason::AuthFailed, attempt_at);
            assert_eq!(ui.attempts(), expected);
        }

        for c in "again".chars() {
            ui.push_char(c);
        }
        let after = now + Duration::from_secs(1);
        assert!(ui.take_request(after).is_none(), "連続失敗後は待たせる");
        assert_eq!(ui.status_line(after), "WAIT 5s");
        assert!(ui.take_request(after + Duration::from_secs(5)).is_some());
    }

    #[test]
    fn max_attempts_zero_disables_cooldown() {
        let now = Instant::now();
        let mut ui = UiState::new(
            &UiConfig {
                max_attempts: 0,
                ..UiConfig::default()
            },
            &AuthConfig::default(),
        );
        ui.apply_status(true, &[AuthKind::Password]);
        for _ in 0..10 {
            ui.push_char('x');
            assert!(ui.take_request(now).is_some());
            ui.finish_failure(Reason::AuthFailed, now);
        }
        assert_eq!(ui.status_line(now), "AUTH FAILED");
        ui.push_char('x');
        assert!(ui.take_request(now).is_some());
    }

    #[test]
    fn no_card_switches_to_password_when_possible() {
        let mut ui = state();
        ui.apply_status(true, &[AuthKind::Pin, AuthKind::Password]);
        assert_eq!(ui.mode(), Mode::Pin);

        ui.finish_failure(Reason::NoCard, Instant::now());
        assert_eq!(ui.mode(), Mode::Password);
        assert_eq!(ui.status_line(Instant::now()), "CARD NOT FOUND");
        assert_eq!(ui.attempts(), 0);
    }

    #[test]
    fn no_card_keeps_pin_when_password_is_disabled() {
        let mut ui = state();
        ui.apply_status(false, &[AuthKind::Pin]);
        assert_eq!(ui.mode(), Mode::Pin);
        ui.finish_failure(Reason::NoCard, Instant::now());
        assert_eq!(ui.mode(), Mode::Pin);
        assert_eq!(ui.status_line(Instant::now()), "CARD NOT FOUND");
    }

    #[test]
    fn disabled_method_failure_moves_to_the_other_mode() {
        let mut ui = state();
        ui.finish_failure(Reason::PinDisabled, Instant::now());
        assert_eq!(ui.mode(), Mode::Password);

        let mut ui = state();
        ui.finish_failure(Reason::PasswordDisabled, Instant::now());
        assert_eq!(ui.mode(), Mode::Pin);
    }

    #[test]
    fn helper_unavailable_keeps_input_empty_and_shows_message() {
        let mut ui = state();
        ui.push_char('x');
        ui.helper_unavailable();
        assert_eq!(ui.secret(), "");
        assert_eq!(ui.status_line(Instant::now()), "AUTH HELPER UNAVAILABLE");
        assert!(!ui.is_busy());
    }

    #[test]
    fn config_error_is_reported_without_unlocking() {
        let mut ui = state();
        ui.push_char('x');
        ui.config_error();
        assert_eq!(ui.secret(), "");
        assert_eq!(ui.status_line(Instant::now()), "CONFIG ERROR - LOCKED");
        assert!(!ui.is_busy());
    }
}
