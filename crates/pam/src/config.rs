//! `/etc/nekonolock/sudo.toml` の型と検証。
//!
//! このファイルは root 所有でなければならない。ユーザーの
//! `~/.config/nekonolock/config.toml` は PAM モジュールからは読まない
//! （ユーザーが書き換えられる設定を権限境界に使わない）。

use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

/// PAM モジュールが既定で読む設定。
pub const DEFAULT_CONFIG_PATH: &str = "/etc/nekonolock/sudo.toml";

/// 設定ファイルの上限。壊れた・巨大なファイルで PAM を止めない。
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// sudo 用の設定ファイル全体。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SudoFile {
    pub sudo: SudoConfig,
}

/// sudo の OpenPGP カード認証。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SudoConfig {
    /// false の間はモジュールが何もしない（パスワード認証へそのまま進む）。
    pub enabled: bool,
    /// OpenPGP の鍵。16 桁の long key ID か 40 桁の fingerprint。
    pub key: String,
    /// 検証に使う公開鍵（ASCII armored）。root 所有。
    pub public_key: PathBuf,
    pub challenge_bytes: usize,
    pub timeout_sec: u64,
    /// 空なら全ユーザー。名前を並べたときだけ、そのユーザーに限定する。
    pub users: Vec<String>,
}

impl Default for SudoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            key: String::new(),
            public_key: PathBuf::from("/etc/nekonolock/sudo-key.asc"),
            challenge_bytes: 32,
            timeout_sec: 30,
            users: Vec::new(),
        }
    }
}

impl SudoFile {
    /// 明示パスから読む。root 所有・他人が書けない通常ファイルだけを受け付ける。
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = read_trusted_text(path, MAX_CONFIG_BYTES)?;
        let file: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        file.validate()?;
        Ok(file)
    }

    /// パースだけを行う（テストと、呼び出し側が別経路で読んだ場合用）。
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let file: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })?;
        file.validate()?;
        Ok(file)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.sudo.validate()
    }
}

impl SudoConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }

        let key = self.key.trim();
        let key = key.trim_end_matches('!');
        if key.is_empty() {
            return Err(ConfigError::Invalid(
                "[sudo] enabled = true なら key が要る".to_string(),
            ));
        }
        if !key.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ConfigError::Invalid(
                "[sudo] key は 16 桁の long key ID か 40 桁の fingerprint（16 進）にする"
                    .to_string(),
            ));
        }
        if key.len() != 16 && key.len() != 40 {
            return Err(ConfigError::Invalid(
                "[sudo] key は 16 桁の long key ID か 40 桁の fingerprint にする".to_string(),
            ));
        }
        if self.key.chars().any(char::is_whitespace) {
            return Err(ConfigError::Invalid(
                "[sudo] key に空白を含められない".to_string(),
            ));
        }

        if !self.public_key.is_absolute() {
            return Err(ConfigError::Invalid(
                "[sudo] public_key は絶対パスにする".to_string(),
            ));
        }
        if self
            .public_key
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(ConfigError::Invalid(
                "[sudo] public_key に .. を含められない".to_string(),
            ));
        }

        if !(16..=64).contains(&self.challenge_bytes) {
            return Err(ConfigError::Invalid(
                "[sudo] challenge_bytes は 16..=64 にする".to_string(),
            ));
        }
        if !(1..=60).contains(&self.timeout_sec) {
            return Err(ConfigError::Invalid(
                "[sudo] timeout_sec は 1..=60 にする".to_string(),
            ));
        }

        for user in &self.users {
            if !is_safe_user_name(user) {
                return Err(ConfigError::Invalid(format!(
                    "[sudo] users に使えない名前がある: {user}"
                )));
            }
        }

        Ok(())
    }

    /// `users` が空なら全ユーザーを許可する。
    pub fn allows(&self, user: &str) -> bool {
        self.users.is_empty() || self.users.iter().any(|allowed| allowed == user)
    }
}

/// PAM のユーザー名として安全な文字だけを許可する。
pub fn is_safe_user_name(user: &str) -> bool {
    !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '@' | '-'))
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("設定ファイルを読めない: {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("設定ファイルが安全でない: {path}: {reason}")]
    Unsafe { path: PathBuf, reason: String },
    #[error("設定ファイルを解釈できない: {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("設定が不正: {0}")]
    Invalid(String),
}

/// root 所有で、group / other から書き換えられない通常ファイルを読む。
///
/// symlink は辿らない。メタデータは開いた fd に対して確認するので、
/// パスの差し替え（TOCTOU）で別ファイルを読む経路を残さない。
pub fn read_trusted_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ConfigError> {
    check_parent_dir(path)?;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    let metadata = file.metadata().map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(ConfigError::Unsafe {
            path: path.to_path_buf(),
            reason: "通常ファイルではない".to_string(),
        });
    }
    if metadata.uid() != 0 {
        return Err(ConfigError::Unsafe {
            path: path.to_path_buf(),
            reason: "所有者が root ではない".to_string(),
        });
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(ConfigError::Unsafe {
            path: path.to_path_buf(),
            reason: "group / other に書き込み権がある".to_string(),
        });
    }
    if metadata.len() > max_bytes {
        return Err(ConfigError::Unsafe {
            path: path.to_path_buf(),
            reason: format!("大きすぎる（上限 {max_bytes} byte）"),
        });
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(ConfigError::Unsafe {
            path: path.to_path_buf(),
            reason: "読み込み中に大きくなった".to_string(),
        });
    }
    Ok(bytes)
}

fn read_trusted_text(path: &Path, max_bytes: u64) -> Result<String, ConfigError> {
    let bytes = read_trusted_file(path, max_bytes)?;
    String::from_utf8(bytes).map_err(|_| ConfigError::Unsafe {
        path: path.to_path_buf(),
        reason: "UTF-8 ではない".to_string(),
    })
}

/// 親ディレクトリが root 所有で、group / other から書き換えられないことを確認する。
/// `/etc/nekonolock` のような専用ディレクトリを想定する。
fn check_parent_dir(path: &Path) -> Result<(), ConfigError> {
    let parent = path.parent().ok_or_else(|| ConfigError::Unsafe {
        path: path.to_path_buf(),
        reason: "親ディレクトリが無い".to_string(),
    })?;
    let metadata = std::fs::symlink_metadata(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(ConfigError::Unsafe {
            path: parent.to_path_buf(),
            reason: "ディレクトリではない".to_string(),
        });
    }
    if metadata.uid() != 0 {
        return Err(ConfigError::Unsafe {
            path: parent.to_path_buf(),
            reason: "所有者が root ではない".to_string(),
        });
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(ConfigError::Unsafe {
            path: parent.to_path_buf(),
            reason: "group / other に書き込み権がある".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> String {
        r#"
[sudo]
enabled = true
key = "030130E2F156CD74"
public_key = "/etc/nekonolock/sudo-key.asc"
"#
        .to_string()
    }

    #[test]
    fn disabled_by_default() {
        let file = SudoFile::parse("").unwrap();
        assert!(!file.sudo.enabled);
        assert!(file.sudo.allows("anyone"));
    }

    #[test]
    fn parses_enabled_config() {
        let file = SudoFile::parse(&minimal()).unwrap();
        assert!(file.sudo.enabled);
        assert_eq!(file.sudo.challenge_bytes, 32);
        assert_eq!(file.sudo.timeout_sec, 30);
        assert!(file.sudo.allows("tester"));
    }

    #[test]
    fn rejects_invalid_keys_and_paths() {
        for key in ["", "1234", "zz0130E2F156CD74", "030130E2F156CD7"] {
            let text = minimal().replace("030130E2F156CD74", key);
            assert!(SudoFile::parse(&text).is_err(), "key={key}");
        }

        let text = minimal().replace("/etc/nekonolock/sudo-key.asc", "relative/sudo-key.asc");
        assert!(SudoFile::parse(&text).is_err());

        let text = minimal().replace("/etc/nekonolock/sudo-key.asc", "/etc/../x");
        assert!(SudoFile::parse(&text).is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_bad_limits() {
        assert!(
            SudoFile::parse("[sudo]\nenabled = true\nkey = \"030130E2F156CD74\"\nextra = 1\n")
                .is_err()
        );

        let text = format!("{}\nchallenge_bytes = 15\n", minimal());
        assert!(SudoFile::parse(&text).is_err());
        let text = format!("{}\nchallenge_bytes = 65\n", minimal());
        assert!(SudoFile::parse(&text).is_err());
        let text = format!("{}\ntimeout_sec = 0\n", minimal());
        assert!(SudoFile::parse(&text).is_err());
        let text = format!("{}\ntimeout_sec = 61\n", minimal());
        assert!(SudoFile::parse(&text).is_err());
    }

    #[test]
    fn restricts_users_when_listed() {
        let text = format!("{}\nusers = [\"alice\", \"bob\"]\n", minimal());
        let file = SudoFile::parse(&text).unwrap();
        assert!(file.sudo.allows("alice"));
        assert!(!file.sudo.allows("root"));
    }

    #[test]
    fn user_names_are_conservative() {
        assert!(is_safe_user_name("alice"));
        assert!(is_safe_user_name("alice.bob@example"));
        assert!(!is_safe_user_name(""));
        assert!(!is_safe_user_name("a/b"));
        assert!(!is_safe_user_name("a\nb"));
    }
}
