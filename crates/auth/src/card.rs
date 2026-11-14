//! OpenPGP applet によるチャレンジ署名。
//!
//! カードを握るのは scdaemon だけにしたいので、ここでは `gpg` を子プロセスとして
//! 呼ぶ。PIN は argv ではなく stdin（`--passphrase-fd 0`）で渡す。

use std::fs;
use std::io::{self, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

/// カード署名の抽象。テストではモックに差し替える。
pub trait CardSigner {
    fn present(&self) -> bool;
    fn sign(&self, challenge: &[u8], pin: &str) -> Result<Vec<u8>, CardError>;
    fn verify(&self, challenge: &[u8], signature: &[u8]) -> Result<bool, CardError>;
}

#[derive(Debug, thiserror::Error)]
pub enum CardError {
    #[error("PIN が正しくない")]
    BadPin,
    #[error("gpg を実行できない: {0}")]
    Unavailable(String),
    #[error("gpg がタイムアウトした")]
    Timeout,
    #[error("署名ファイルを扱えない: {0}")]
    Io(#[from] io::Error),
}

/// `gpg` を呼ぶ実装。カードへのアクセスはすべて scdaemon 経由になる。
#[derive(Debug, Clone)]
pub struct GpgCard {
    key: String,
    timeout: Duration,
}

impl GpgCard {
    pub fn new(key: impl Into<String>, timeout: Duration) -> Self {
        Self {
            key: normalize_key(&key.into()),
            timeout,
        }
    }

    /// `!` を付けて primary を固定する。付けないと署名 subkey に解決され、
    /// 別のカードを要求することがある。
    fn local_user(&self) -> String {
        if self.key.ends_with('!') {
            self.key.clone()
        } else {
            format!("{}!", self.key)
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new("gpg");
        // 出力の文言を分類に使うため、ロケールを固定する。
        command.env("LC_ALL", "C").env("LANG", "C");
        command
    }
}

impl CardSigner for GpgCard {
    fn present(&self) -> bool {
        let mut command = self.command();
        command.args(["--batch", "--no-tty", "--card-status"]);

        match run_command(command, None, self.timeout.min(Duration::from_secs(10))) {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                self.key.is_empty() || key_matches(&text, &self.key)
            }
            _ => false,
        }
    }

    fn sign(&self, challenge: &[u8], pin: &str) -> Result<Vec<u8>, CardError> {
        let dir = tempfile::Builder::new()
            .prefix("nekonolock-sign-")
            .tempdir()?;
        let challenge_path = dir.path().join("challenge");
        let signature_path = dir.path().join("challenge.sig");
        fs::write(&challenge_path, challenge)?;

        let mut command = self.command();
        command
            .args([
                "--batch",
                "--yes",
                "--no-tty",
                "--local-user",
                &self.local_user(),
                "--detach-sign",
                "--pinentry-mode",
                "loopback",
                "--passphrase-fd",
                "0",
                "--output",
            ])
            .arg(&signature_path)
            .arg(&challenge_path);

        // PIN は改行で終端する。argv には出さない。使い終わったら zeroize する。
        let stdin = Zeroizing::new(format!("{pin}\n"));
        let output = match run_command(command, Some(stdin.as_bytes()), self.timeout) {
            Ok(output) => output,
            Err(RunError::Timeout) => return Err(CardError::Timeout),
            Err(RunError::Io(err)) => return Err(CardError::Unavailable(err.to_string())),
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(classify_sign_failure(&stderr));
        }

        let signature = fs::read(&signature_path)?;
        if signature.is_empty() {
            return Err(CardError::Unavailable("gpg が空の署名を返した".to_string()));
        }
        Ok(signature)
    }

    fn verify(&self, challenge: &[u8], signature: &[u8]) -> Result<bool, CardError> {
        let dir = tempfile::Builder::new()
            .prefix("nekonolock-verify-")
            .tempdir()?;
        let challenge_path = dir.path().join("challenge");
        let signature_path = dir.path().join("challenge.sig");
        fs::write(&challenge_path, challenge)?;
        fs::write(&signature_path, signature)?;

        let mut command = self.command();
        command
            .args(["--batch", "--no-tty", "--status-fd", "1", "--verify"])
            .arg(&signature_path)
            .arg(&challenge_path);

        let output = match run_command(command, None, self.timeout) {
            Ok(output) => output,
            Err(RunError::Timeout) => return Err(CardError::Timeout),
            Err(RunError::Io(err)) => return Err(CardError::Unavailable(err.to_string())),
        };

        // 署名が壊れている・鍵が無い場合は false。実行自体の失敗は gpg-error。
        if !output.status.success() {
            return Ok(false);
        }

        let status = String::from_utf8_lossy(&output.stdout);
        match parse_validsig(&status) {
            Some(fingerprint) => Ok(key_matches(fingerprint, &self.key)),
            None => Ok(false),
        }
    }
}

fn normalize_key(key: &str) -> String {
    key.trim().trim_end_matches('!').to_string()
}

/// `gpg --card-status` の出力や VALIDSIG の fingerprint と設定鍵を比べる。
/// 設定は短い key ID でもよいので、空白を除いて大文字化したうえで照合する。
fn key_matches(candidate: &str, key: &str) -> bool {
    let key = normalize_key(key).to_ascii_uppercase();
    if key.is_empty() {
        return true;
    }
    let candidate: String = candidate
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    candidate.contains(&key)
}

fn parse_validsig(status_output: &str) -> Option<&str> {
    status_output.lines().find_map(|line| {
        let rest = line.strip_prefix("[GNUPG:] VALIDSIG ")?;
        rest.split_whitespace().next()
    })
}

fn classify_sign_failure(stderr: &str) -> CardError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("bad pin")
        || lower.contains("invalid pin")
        || lower.contains("pin blocked")
        || lower.contains("card is blocked")
    {
        return CardError::BadPin;
    }
    // カードの詳細や鍵の情報を返さないよう、最後の 1 行だけにする。
    let last = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("gpg が失敗した");
    CardError::Unavailable(last.trim().to_string())
}

#[derive(Debug)]
enum RunError {
    Io(io::Error),
    Timeout,
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// 子プロセスを実行し、`timeout` を超えたら kill する。
/// 出力は別スレッドで読む（パイプが詰まって待ち続けるのを避ける）。
fn run_command(
    mut command: Command,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<CommandOutput, RunError> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunError::Io)?;

    if let Some(input) = stdin {
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| RunError::Io(io::Error::other("子プロセスの stdin を開けない")))?;
        pipe.write_all(input).map_err(RunError::Io)?;
        // 閉じることで gpg が passphrase を読み終える。
        drop(pipe);
    } else {
        drop(child.stdin.take());
    }

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunError::Io(io::Error::other("子プロセスの stdout を開けない")))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunError::Io(io::Error::other("子プロセスの stderr を開けない")))?;

    let stdout_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let result = stdout.read_to_end(&mut buf);
        (buf, result)
    });
    let stderr_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let result = stderr.read_to_end(&mut buf);
        (buf, result)
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(RunError::Io)? {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Timeout);
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    };

    let (stdout, stdout_result) = stdout_reader
        .join()
        .map_err(|_| RunError::Io(io::Error::other("stdout の読み取りスレッドが落ちた")))?;
    let (stderr, stderr_result) = stderr_reader
        .join()
        .map_err(|_| RunError::Io(io::Error::other("stderr の読み取りスレッドが落ちた")))?;
    stdout_result.map_err(RunError::Io)?;
    stderr_result.map_err(RunError::Io)?;

    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_user_pins_primary() {
        let card = GpgCard::new("ABCD1234", Duration::from_secs(1));
        assert_eq!(card.local_user(), "ABCD1234!");
        let card = GpgCard::new("ABCD1234!", Duration::from_secs(1));
        assert_eq!(card.local_user(), "ABCD1234!");
        assert_eq!(card.key, "ABCD1234");
    }

    #[test]
    fn key_matches_short_and_full_ids() {
        let output = "General key info..: pub  ed25519/030130E2F156CD74 2024-01-01";
        assert!(key_matches(output, "030130E2F156CD74!"));
        assert!(key_matches(output, "030130e2f156cd74"));
        assert!(!key_matches(output, "FFFFFFFFFFFFFFFF"));

        let full = "3B1F 0000 1111 2222 3333  4444 0301 30E2 F156 CD74";
        assert!(key_matches(full, "030130E2F156CD74"));
        assert!(key_matches(
            full,
            "3B1F00001111222233334444030130E2F156CD74"
        ));
    }

    #[test]
    fn parse_validsig_fingerprint() {
        let status = "\
[GNUPG:] NEWSIG
[GNUPG:] KEY_CONSIDERED 030130E2F156CD74 0
[GNUPG:] GOODSIG 1234 Nekono
[GNUPG:] VALIDSIG 3B1F00001111222233334444030130E2F156CD74 2026-11-15 1 0 4 0 22 8 00 3B1F00001111222233334444030130E2F156CD74
[GNUPG:] TRUST_ULTIMATE 0 pgp
";
        assert_eq!(
            parse_validsig(status),
            Some("3B1F00001111222233334444030130E2F156CD74")
        );
        assert_eq!(parse_validsig("[GNUPG:] BADSIG 1234 Nekono"), None);
    }

    #[test]
    fn classifies_bad_pin_and_other_errors() {
        assert!(matches!(
            classify_sign_failure("gpg: signing failed: Bad PIN"),
            CardError::BadPin
        ));
        assert!(matches!(
            classify_sign_failure("gpg: signing failed: Card is blocked"),
            CardError::BadPin
        ));
        let err =
            classify_sign_failure("gpg: selecting card failed: No such device\ngpg: no card\n");
        match err {
            CardError::Unavailable(message) => {
                assert_eq!(message, "gpg: no card");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn normalize_key_strips_suffix() {
        assert_eq!(normalize_key(" ABCD! "), "ABCD");
        assert_eq!(normalize_key(""), "");
    }
}
