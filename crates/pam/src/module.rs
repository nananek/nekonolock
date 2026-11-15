//! `pam_sm_authenticate` の実装。
//!
//! 1. `/etc/nekonolock/sudo.toml`（root 所有）を読む。
//! 2. 実 uid のユーザーとしてカードの有無を確認する。
//! 3. PIN を PAM の会話で聞き、ユーザーの gpg-agent / scdaemon に署名させる。
//! 4. root 側の一時 keyring に取り込んだ信頼した公開鍵で検証する。
//!
//! 成功以外は `PAM_IGNORE` を返し、`auth include system-auth` などの
//! パスワード認証へフォールバックする。ユーザーの
//! `~/.config/nekonolock/config.toml` やユーザーが起動したヘルパは信頼しない。

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::time::Duration;

use nekonolock_gpg::{CardError, CardSigner, GpgCard};

use crate::auth::{Outcome, authenticate};
use crate::config::{ConfigError, DEFAULT_CONFIG_PATH, SudoFile, read_trusted_file};
use crate::pam_ffi::{self, LogLevel, PAM_IGNORE, PAM_SUCCESS};
use crate::user;

const GPG_PATH: &str = "/usr/bin/gpg";
const MAX_PUBLIC_KEY_BYTES: u64 = 1024 * 1024;
const PAM_SILENT: c_int = 0x8000;
const MAX_PIN_BYTES: usize = 512;

struct Options {
    config: PathBuf,
    debug: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn pam_sm_authenticate(
    pamh: *mut c_void,
    flags: c_int,
    argc: c_int,
    argv: *const *const c_char,
) -> c_int {
    match catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: libpam はモジュールに有効な pamh と argv を渡す。
        unsafe { authenticate_inner(pamh, flags, argc, argv) }
    })) {
        Ok(code) => code,
        Err(_) => {
            pam_ffi::log(
                LogLevel::Error,
                "pam_nekonolock: 内部エラーで panic した。パスワードへフォールバックする",
            );
            PAM_IGNORE
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pam_sm_setcred(
    _pamh: *mut c_void,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    // 証明書を設定するモジュールではない。パスワード側の setcred を妨げない。
    PAM_IGNORE
}

/// SAFETY: `pamh` と `argv` は libpam が渡した有効な値であること。
unsafe fn authenticate_inner(
    pamh: *mut c_void,
    flags: c_int,
    argc: c_int,
    argv: *const *const c_char,
) -> c_int {
    let options = match unsafe { parse_options(argc, argv) } {
        Ok(options) => options,
        Err(message) => {
            pam_ffi::log(LogLevel::Error, &format!("pam_nekonolock: {message}"));
            return PAM_IGNORE;
        }
    };

    let file = match SudoFile::load(&options.config) {
        Ok(file) => file,
        Err(ConfigError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            // 設定が無いホストではモジュールを入れても何もしない。
            if options.debug {
                pam_ffi::log(LogLevel::Info, "pam_nekonolock: 設定が無いので何もしない");
            }
            return PAM_IGNORE;
        }
        Err(err) => {
            pam_ffi::log(
                LogLevel::Error,
                &format!("pam_nekonolock: 設定を読めない: {err}"),
            );
            return PAM_IGNORE;
        }
    };

    if !file.sudo.enabled {
        return PAM_IGNORE;
    }

    // SAFETY: getuid は引数を取らない。
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        // root 自身の sudo に YubiKey を要求しない。
        return PAM_IGNORE;
    }

    let identity = match user::lookup(uid) {
        Ok(identity) => identity,
        Err(err) => {
            pam_ffi::log(
                LogLevel::Warning,
                &format!("pam_nekonolock: ユーザーを確認できない: {err}"),
            );
            return PAM_IGNORE;
        }
    };
    if !file.sudo.allows(&identity.name) {
        pam_ffi::log(
            LogLevel::Warning,
            &format!(
                "pam_nekonolock: {} は sudo の YubiKey 認証を許可されていない",
                identity.name
            ),
        );
        return PAM_IGNORE;
    }

    let public_key = match read_trusted_file(&file.sudo.public_key, MAX_PUBLIC_KEY_BYTES) {
        Ok(bytes) => bytes,
        Err(err) => {
            pam_ffi::log(
                LogLevel::Error,
                &format!("pam_nekonolock: 公開鍵を読めない: {err}"),
            );
            return PAM_IGNORE;
        }
    };

    let timeout = Duration::from_secs(file.sudo.timeout_sec);
    let signer = GpgCard::new(&file.sudo.key, timeout)
        .with_program(GPG_PATH)
        .with_identity(identity.clone())
        .with_no_options(true)
        .with_scrubbed_env(true);

    if !signer.present() {
        if options.debug {
            pam_ffi::log(
                LogLevel::Info,
                &format!("pam_nekonolock: {} のカードが見つからない", identity.name),
            );
        }
        return PAM_IGNORE;
    }

    // SAFETY: pamh は libpam が渡した有効なハンドル。
    let pin = match unsafe { pam_ffi::ask(pamh, "YubiKey PIN (empty for password): ") } {
        Ok(Some(pin)) => pin,
        Ok(None) => {
            // 空 Enter やキャンセルはパスワードへ回す。
            return PAM_IGNORE;
        }
        Err(err) => {
            pam_ffi::log(
                LogLevel::Warning,
                &format!("pam_nekonolock: PIN を聞けない: {err}"),
            );
            return PAM_IGNORE;
        }
    };
    if pin.is_empty() || pin.len() > MAX_PIN_BYTES {
        return PAM_IGNORE;
    }

    let verifier_dir = match tempfile::Builder::new().prefix("nekonolock-pam-").tempdir() {
        Ok(dir) => dir,
        Err(err) => {
            pam_ffi::log(
                LogLevel::Error,
                &format!("pam_nekonolock: 一時ディレクトリを作れない: {err}"),
            );
            return PAM_IGNORE;
        }
    };
    let verifier = GpgCard::new(&file.sudo.key, timeout)
        .with_program(GPG_PATH)
        .with_homedir(verifier_dir.path())
        .with_no_options(true)
        .with_scrubbed_env(true)
        .with_full_root(true);
    if let Err(err) = verifier.import_key_bytes(&public_key) {
        pam_ffi::log(
            LogLevel::Error,
            &format!("pam_nekonolock: 公開鍵を取り込めない: {err}"),
        );
        return PAM_IGNORE;
    }

    let card = SplitCard {
        signer: &signer,
        verifier: &verifier,
    };
    let outcome = authenticate(&card, file.sudo.challenge_bytes, pin.as_str());

    match outcome {
        Outcome::Unlocked => {
            pam_ffi::log(
                LogLevel::Info,
                &format!(
                    "pam_nekonolock: {} の sudo を YubiKey で認証した",
                    identity.name
                ),
            );
            PAM_SUCCESS
        }
        Outcome::WrongPin => {
            if options.debug || flags & PAM_SILENT == 0 {
                // SAFETY: pamh は有効。
                unsafe {
                    pam_ffi::message(
                        pamh,
                        "YubiKey PIN was rejected. Falling back to password.",
                        true,
                    );
                }
            }
            pam_ffi::log(
                LogLevel::Warning,
                &format!(
                    "pam_nekonolock: {} の YubiKey PIN を拒否した",
                    identity.name
                ),
            );
            PAM_IGNORE
        }
        Outcome::Error(message) => {
            pam_ffi::log(
                LogLevel::Error,
                &format!("pam_nekonolock: カード認証に失敗した: {message}"),
            );
            if flags & PAM_SILENT == 0 {
                // SAFETY: pamh は有効。
                unsafe {
                    pam_ffi::message(
                        pamh,
                        "YubiKey authentication is unavailable. Falling back to password.",
                        true,
                    );
                }
            }
            PAM_IGNORE
        }
    }
}

/// 署名はユーザー側、検証は root 側の別インスタンスで行う。
struct SplitCard<'a, S: CardSigner, V: CardSigner> {
    signer: &'a S,
    verifier: &'a V,
}

impl<'a, S: CardSigner, V: CardSigner> CardSigner for SplitCard<'a, S, V> {
    fn present(&self) -> bool {
        self.signer.present()
    }

    fn sign(&self, challenge: &[u8], pin: &str) -> Result<Vec<u8>, CardError> {
        self.signer.sign(challenge, pin)
    }

    fn verify(&self, challenge: &[u8], signature: &[u8]) -> Result<bool, CardError> {
        self.verifier.verify(challenge, signature)
    }
}

/// SAFETY: `argv` は argc 個の有効な C 文字列へのポインタ配列であること。
unsafe fn parse_options(argc: c_int, argv: *const *const c_char) -> Result<Options, String> {
    let mut options = Options {
        config: PathBuf::from(DEFAULT_CONFIG_PATH),
        debug: false,
    };
    if argc < 0 {
        return Err("引数の数が負".to_string());
    }
    if argc > 0 && argv.is_null() {
        return Err("引数配列が null".to_string());
    }

    for index in 0..argc as usize {
        // SAFETY: argv は argc 個のポインタを持つ。
        let pointer = unsafe { *argv.add(index) };
        if pointer.is_null() {
            return Err("引数が null".to_string());
        }
        // SAFETY: pointer は NUL 終端文字列。
        let arg = unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .map_err(|_| "引数が UTF-8 ではない".to_string())?;
        if arg == "debug" {
            options.debug = true;
        } else if let Some(path) = arg.strip_prefix("config=") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("config= は絶対パスにする".to_string());
            }
            options.config = path;
        } else {
            return Err(format!("知らないモジュール引数: {arg}"));
        }
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_options() {
        let config = c"config=/tmp/x".as_ptr();
        let debug = c"debug".as_ptr();
        let argv = [config, debug];
        // SAFETY: テスト用の静的な C 文字列配列。
        let options = unsafe { parse_options(2, argv.as_ptr()) }.unwrap();
        assert_eq!(options.config, PathBuf::from("/tmp/x"));
        assert!(options.debug);
    }

    #[test]
    fn rejects_unknown_and_relative_options() {
        let arg = c"config=relative".as_ptr();
        let argv = [arg];
        // SAFETY: テスト用の静的な C 文字列配列。
        assert!(unsafe { parse_options(1, argv.as_ptr()) }.is_err());

        let arg = c"unknown".as_ptr();
        let argv = [arg];
        // SAFETY: 同上。
        assert!(unsafe { parse_options(1, argv.as_ptr()) }.is_err());
    }
}
