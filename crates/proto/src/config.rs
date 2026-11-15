//! `~/.config/nekonolock/config.toml` の型と検証。
//!
//! UI と認証ヘルパの両方が読むため、既定値と検証をここに集約する。

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 設定ファイル全体。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub ui: UiConfig,
    pub auth: AuthConfig,
    pub pam: PamConfig,
}

/// ロック画面の見た目と操作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub clock_format: String,
    pub font: String,
    pub background: String,
    pub foreground: String,
    pub dim: String,
    /// UI 側の連打抑止。0 で無効。PIN の自動再試行には使わない。
    pub max_attempts: u32,
    pub frame_width: u32,
    pub pin_accent: String,
    pub pin_on_accent: String,
    pub password_accent: String,
    pub password_on_accent: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            clock_format: "%Y-%m-%d (%a) %H:%M".to_string(),
            font: "/usr/share/fonts/TTF/DejaVuSansMono.ttf".to_string(),
            background: "#000000".to_string(),
            foreground: "#eeeeee".to_string(),
            dim: "#888888".to_string(),
            max_attempts: 5,
            frame_width: 12,
            pin_accent: "#1b2a6b".to_string(),
            pin_on_accent: "#ffd400".to_string(),
            password_accent: "#7a1024".to_string(),
            password_on_accent: "#ffffff".to_string(),
        }
    }
}

/// 認証方式の設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub openpgp: bool,
    pub password: bool,
    /// OpenPGP の鍵。`!` は primary 固定のため実装側で付ける。
    pub key: String,
    pub challenge_bytes: usize,
    pub timeout_sec: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            openpgp: true,
            password: true,
            key: "030130E2F156CD74!".to_string(),
            challenge_bytes: 32,
            timeout_sec: 60,
        }
    }
}

/// PAM の設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PamConfig {
    pub service: String,
}

impl Default for PamConfig {
    fn default() -> Self {
        Self {
            service: "nekonolock".to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("設定ファイルを読めない: {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("設定ファイルを解釈できない: {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("設定が不正: {0}")]
    Invalid(String),
}

impl Config {
    /// 明示パス、なければ `$XDG_CONFIG_HOME/nekonolock/config.toml`
    /// （未設定なら `$HOME/.config/nekonolock/config.toml`）を読む。
    /// ファイルが無い場合は既定値を返す。UI と認証の両方を検証する。
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let config = Self::load_unchecked(path)?;
        config.validate()?;
        Ok(config)
    }

    /// UI 用。`[ui]` だけを検証し、`[auth]` / `[pam]` の型・未知項目の誤りで
    /// ロック画面が起動できなくなることを避ける。認証設定として解釈できる項目
    /// だけを使い、読めない項目は既定値に落とす。
    pub fn load_ui(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = resolve_path(path)?;
        let Some(text) = read_text(&path)? else {
            let config = Config::default();
            config.validate_ui()?;
            return Ok(config);
        };

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct UiFile {
            #[serde(default)]
            ui: UiConfig,
            #[serde(default)]
            auth: Option<toml::Value>,
            #[serde(default)]
            pam: Option<toml::Value>,
        }

        let file: UiFile =
            toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })?;
        let config = Config {
            ui: file.ui,
            auth: file
                .auth
                .and_then(|value| value.try_into::<AuthConfig>().ok())
                .unwrap_or_default(),
            pam: file
                .pam
                .and_then(|value| value.try_into::<PamConfig>().ok())
                .unwrap_or_default(),
        };
        config.validate_ui()?;
        Ok(config)
    }

    /// 認証ヘルパ用。`[auth]` と `[pam]` だけを検証し、`[ui]` の誤りで
    /// ロック解除できなくなることを避ける。`[ui]` は解釈もしない。
    pub fn load_auth(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = resolve_path(path)?;
        let Some(text) = read_text(&path)? else {
            let config = Config::default();
            config.validate_auth()?;
            return Ok(config);
        };

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct AuthFile {
            #[serde(default)]
            auth: AuthConfig,
            #[serde(default)]
            pam: PamConfig,
            // UI の値は検証しないが、トップレベルの綴り間違いは弾く。
            #[serde(default, rename = "ui")]
            _ui: Option<toml::Value>,
        }

        let file: AuthFile =
            toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })?;
        let config = Config {
            ui: UiConfig::default(),
            auth: file.auth,
            pam: file.pam,
        };
        config.validate_auth()?;
        Ok(config)
    }

    fn load_unchecked(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = resolve_path(path)?;
        match read_text(&path)? {
            Some(text) => {
                toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })
            }
            None => Ok(Config::default()),
        }
    }

    /// UI と認証の両方を検証する。
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_auth()?;
        self.validate_ui()
    }

    /// 認証ヘルパが起動時に拒否すべき設定を弾く。
    pub fn validate_auth(&self) -> Result<(), ConfigError> {
        if !self.auth.openpgp && !self.auth.password {
            return Err(ConfigError::Invalid(
                "[auth] の openpgp と password を両方 false にはできない".to_string(),
            ));
        }

        if self.auth.openpgp {
            let key = self.auth.key.trim();
            let key = key.trim_end_matches('!');
            if key.is_empty() {
                return Err(ConfigError::Invalid(
                    "[auth] openpgp = true なら key が要る".to_string(),
                ));
            }
            if !key.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ConfigError::Invalid(
                    "[auth] key は 16 桁の long key ID か 40 桁の fingerprint（16 進）にする"
                        .to_string(),
                ));
            }
            if key.len() != 16 && key.len() != 40 {
                return Err(ConfigError::Invalid(
                    "[auth] key は 16 桁の long key ID か 40 桁の fingerprint にする".to_string(),
                ));
            }
            if self.auth.key.chars().any(char::is_whitespace) {
                return Err(ConfigError::Invalid(
                    "[auth] key に空白を含められない".to_string(),
                ));
            }
        }

        if !(16..=64).contains(&self.auth.challenge_bytes) {
            return Err(ConfigError::Invalid(
                "[auth] challenge_bytes は 16..=64 にする".to_string(),
            ));
        }

        if !(1..=600).contains(&self.auth.timeout_sec) {
            return Err(ConfigError::Invalid(
                "[auth] timeout_sec は 1..=600 にする".to_string(),
            ));
        }

        if self.pam.service.trim().is_empty() || self.pam.service.contains('/') {
            return Err(ConfigError::Invalid(
                "[pam] service は空でないファイル名にする".to_string(),
            ));
        }

        Ok(())
    }

    /// ロック画面が起動時に拒否すべき設定を弾く（Phase 3 で使う）。
    pub fn validate_ui(&self) -> Result<(), ConfigError> {
        if self.ui.max_attempts > 100 {
            return Err(ConfigError::Invalid(
                "[ui] max_attempts は 0..=100 にする".to_string(),
            ));
        }

        if self.ui.frame_width > 128 {
            return Err(ConfigError::Invalid(
                "[ui] frame_width は 0..=128 にする".to_string(),
            ));
        }

        for (name, value) in [
            ("background", &self.ui.background),
            ("foreground", &self.ui.foreground),
            ("dim", &self.ui.dim),
            ("pin_accent", &self.ui.pin_accent),
            ("pin_on_accent", &self.ui.pin_on_accent),
            ("password_accent", &self.ui.password_accent),
            ("password_on_accent", &self.ui.password_on_accent),
        ] {
            if !is_hex_color(value) {
                return Err(ConfigError::Invalid(format!(
                    "[ui] {name} は #rrggbb 形式にする"
                )));
            }
        }

        if self.ui.font.trim().is_empty() {
            return Err(ConfigError::Invalid("[ui] font を空にできない".to_string()));
        }

        Ok(())
    }
}

fn resolve_path(path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    match path {
        Some(path) => Ok(path.to_path_buf()),
        None => default_path().ok_or_else(|| {
            ConfigError::Invalid(
                "XDG_CONFIG_HOME も HOME も無いため設定ファイルの場所を決められない".to_string(),
            )
        }),
    }
}

fn read_text(path: &Path) -> Result<Option<String>, ConfigError> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|source| ConfigError::Io {
                path: path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, source),
            }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// `#rrggbb` だけを受け付ける（`#rgb` や名前色は使わない）。
fn is_hex_color(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

fn default_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir.join("nekonolock/config.toml"));
        }
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/nekonolock/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(contents: &str) -> PathBuf {
        // テストごとに衝突しないパスを作る。中身は小さく、後始末は OS に任せる。
        let unique = format!(
            "nekonolock-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = env::temp_dir().join(unique).with_extension("toml");
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn defaults_match_design() {
        let config = Config::default();
        assert!(config.auth.openpgp);
        assert!(config.auth.password);
        assert_eq!(config.auth.key, "030130E2F156CD74!");
        assert_eq!(config.auth.challenge_bytes, 32);
        assert_eq!(config.auth.timeout_sec, 60);
        assert_eq!(config.pam.service, "nekonolock");
        assert_eq!(config.ui.frame_width, 12);
        config.validate().unwrap();
    }

    #[test]
    fn parse_full_config() {
        let path = temp_config(
            r##"
[ui]
clock_format = "%H:%M"
max_attempts = 0
frame_width = 8
background = "#101010"

[auth]
openpgp = false
password = true
key = "ABCDEF0123456789!"
challenge_bytes = 48
timeout_sec = 30

[pam]
service = "nekonolock-test"
"##,
        );
        let config = Config::load(Some(&path)).unwrap();
        fs::remove_file(&path).ok();

        assert!(!config.auth.openpgp);
        assert!(config.auth.password);
        assert_eq!(config.ui.clock_format, "%H:%M");
        assert_eq!(config.ui.max_attempts, 0);
        assert_eq!(config.ui.frame_width, 8);
        assert_eq!(config.ui.background, "#101010");
        assert_eq!(config.auth.challenge_bytes, 48);
        assert_eq!(config.auth.timeout_sec, 30);
        assert_eq!(config.pam.service, "nekonolock-test");
    }

    #[test]
    fn missing_file_uses_defaults() {
        let path = env::temp_dir().join("nekonolock-config-does-not-exist.toml");
        fs::remove_file(&path).ok();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn rejects_both_methods_disabled() {
        let mut config = Config::default();
        config.auth.openpgp = false;
        config.auth.password = false;
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_unknown_field_and_bad_values() {
        let path = temp_config("[auth]\nopenpgp = true\npassword = true\nunknown = 1\n");
        assert!(matches!(
            Config::load(Some(&path)),
            Err(ConfigError::Parse { .. })
        ));
        fs::remove_file(&path).ok();

        let mut config = Config::default();
        config.ui.pin_accent = "blue".to_string();
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.auth.challenge_bytes = 0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.auth.timeout_sec = 0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.auth.key = "  ".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn ui_loading_ignores_semantically_broken_auth_settings() {
        let path = temp_config(
            r##"
[ui]
background = "#101010"

[auth]
openpgp = false
password = false
key = "not-a-key"
"##,
        );
        assert!(matches!(
            Config::load(Some(&path)),
            Err(ConfigError::Invalid(_))
        ));

        let config = Config::load_ui(Some(&path)).unwrap();
        assert_eq!(config.ui.background, "#101010");
        assert!(!config.auth.openpgp);
        assert!(!config.auth.password);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn ui_loading_ignores_unparseable_auth_settings() {
        let path = temp_config(
            r##"
[ui]
background = "#101010"

[auth]
openpgp = true
password = true
timeout_sec = "sixty"
"##,
        );
        let config = Config::load_ui(Some(&path)).unwrap();
        assert_eq!(config.ui.background, "#101010");
        assert_eq!(config.auth.timeout_sec, AuthConfig::default().timeout_sec);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn ui_loading_still_rejects_broken_ui_settings() {
        let path = temp_config(
            r##"
[ui]
background = "blue"
"##,
        );
        assert!(matches!(
            Config::load_ui(Some(&path)),
            Err(ConfigError::Invalid(_))
        ));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn auth_loading_ignores_broken_ui_settings() {
        let path = temp_config(
            r##"
[ui]
background = "blue"

[auth]
openpgp = true
password = true
key = "ABCDEF0123456789!"

[pam]
service = "nekonolock-test"
"##,
        );
        assert!(matches!(
            Config::load(Some(&path)),
            Err(ConfigError::Invalid(_))
        ));
        let config = Config::load_auth(Some(&path)).unwrap();
        assert_eq!(config.auth.key, "ABCDEF0123456789!");
        assert_eq!(config.pam.service, "nekonolock-test");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn auth_loading_rejects_top_level_typos() {
        let path = temp_config(
            "[aut]
openpgp = false
",
        );
        assert!(matches!(
            Config::load_auth(Some(&path)),
            Err(ConfigError::Parse { .. })
        ));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_weak_or_malformed_openpgp_keys() {
        for key in [
            "!",
            "!!",
            "ABCD1234",
            "TESTKEY1234567890",
            "030130E2F156CD7G",
        ] {
            let mut config = Config::default();
            config.auth.key = key.to_string();
            assert!(config.validate_auth().is_err(), "key {key:?}");
        }

        for key in [
            "030130E2F156CD74",
            "030130E2F156CD74!",
            "483DC691DF9F29327EA106BD030130E2F156CD74",
            "483DC691DF9F29327EA106BD030130E2F156CD74!",
        ] {
            let config = AuthConfig {
                key: key.to_string(),
                ..AuthConfig::default()
            };
            let config = Config {
                auth: config,
                ..Config::default()
            };
            config.validate_auth().unwrap();
        }
    }

    #[test]
    fn hex_color_syntax() {
        assert!(is_hex_color("#000000"));
        assert!(is_hex_color("#aBc123"));
        assert!(!is_hex_color("000000"));
        assert!(!is_hex_color("#000"));
        assert!(!is_hex_color("#gggggg"));
        assert!(!is_hex_color("#0000000"));
    }
}
