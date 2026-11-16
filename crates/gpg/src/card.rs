//! OpenPGP applet によるチャレンジ署名。
//!
//! カードを握るのは scdaemon だけにしたいので、ここでは `gpg` を子プロセスとして
//! 呼ぶ。PIN は argv ではなく stdin（`--passphrase-fd 0`）で渡す。

use std::ffi::CStr;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

/// 子プロセスの gpg に渡す challenge / signature の fd。
///
/// gpg にはパスではなく `/proc/self/fd/<n>` を渡し、ユーザーが差し替えられる
/// ディレクトリ内の名前を root がたどらないようにする。
const CHALLENGE_FD: RawFd = 198;
const SIGNATURE_FD: RawFd = 199;

/// 子プロセスを実行するユーザー。PAM モジュールが root から呼ぶときに使う。
///
/// `home` と `runtime_dir` はそのユーザーが所有することを呼び出し側が確認する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
    pub name: String,
    pub home: PathBuf,
    pub runtime_dir: PathBuf,
}

/// 署名に使う開いたファイル。gpg には fd を渡し、このハンドルから読む。
#[derive(Debug)]
struct SignFiles {
    challenge: fs::File,
    signature: fs::File,
}

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
    program: PathBuf,
    identity: Option<Identity>,
    homedir: Option<PathBuf>,
    no_options: bool,
    scrub_env: bool,
    full_root: bool,
}

impl GpgCard {
    pub fn new(key: impl Into<String>, timeout: Duration) -> Self {
        Self {
            key: normalize_key(&key.into()),
            timeout,
            program: PathBuf::from("gpg"),
            identity: None,
            homedir: None,
            no_options: false,
            scrub_env: false,
            full_root: false,
        }
    }

    /// 実行する `gpg` の絶対パス。PAM モジュールは PATH に依存しない。
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// 子プロセスを指定ユーザーとして実行する。root から呼ぶ場合に使う。
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// `gpg --homedir` を固定する。root 側で信頼した鍵だけを検証するために使う。
    pub fn with_homedir(mut self, homedir: impl Into<PathBuf>) -> Self {
        self.homedir = Some(homedir.into());
        self
    }

    /// ユーザー設定（`gpg.conf` など）を読まない。
    pub fn with_no_options(mut self, no_options: bool) -> Self {
        self.no_options = no_options;
        self
    }

    /// 継承した環境変数を捨て、必要なものだけを設定する。
    pub fn with_scrubbed_env(mut self, scrub_env: bool) -> Self {
        self.scrub_env = scrub_env;
        self
    }

    /// 子プロセスの実 uid も root にする。
    ///
    /// sudo の PAM モジュールは実 uid = 呼び出したユーザー、実効 uid = root で
    /// 動く。gpg は実 uid でファイルを開くため、root 所有の一時 keyring を
    /// 読ませるには子プロセスで `setuid(0)` して実 uid も 0 にする必要がある。
    /// 検証用の信頼した鍵だけを扱う子に限定して使う。
    pub fn with_full_root(mut self, full_root: bool) -> Self {
        self.full_root = full_root;
        self
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

    /// 子プロセスの環境と実行ユーザーを設定する。gpg と gpg-connect-agent で共有する。
    fn base_command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        if self.scrub_env {
            command.env_clear();
            command.env("PATH", "/usr/bin:/bin");
        }
        // 出力の文言を分類に使うため、ロケールを固定する。
        command.env("LC_ALL", "C").env("LANG", "C");

        if let Some(identity) = &self.identity {
            command
                .env("HOME", &identity.home)
                .env("USER", &identity.name)
                .env("LOGNAME", &identity.name)
                .env("XDG_RUNTIME_DIR", &identity.runtime_dir)
                .env("GNUPGHOME", identity.home.join(".gnupg"));
            switch_identity(&mut command, identity.uid, identity.gid);
        }

        if self.full_root {
            switch_identity(&mut command, 0, 0);
        }
        command
    }

    fn command(&self) -> Command {
        let mut command = self.base_command(&self.program);
        if let Some(homedir) = &self.homedir {
            command.arg("--homedir").arg(homedir);
        }
        if self.no_options {
            command.arg("--no-options");
        }
        command
    }

    /// 署名用の一時ディレクトリを作る。
    ///
    /// 子プロセスを別ユーザーに落とす場合、root が作った 0700 の `/tmp` 配下
    /// にはそのユーザーが入れない。そこでユーザーの runtime dir の中に作る。
    /// ただし runtime dir はユーザー所有なので、このディレクトリは作成後に
    /// rename / symlink へ差し替えられる。パスを信頼せず、ファイル操作は
    /// 開いた fd だけを基準にする。
    fn private_tempdir(&self, prefix: &str) -> Result<tempfile::TempDir, CardError> {
        match &self.identity {
            Some(identity) => Ok(tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(&identity.runtime_dir)?),
            None => Ok(tempfile::Builder::new().prefix(prefix).tempdir()?),
        }
    }

    /// 署名対象と署名先を、ディレクトリの fd を基準に新規作成する。
    ///
    /// 既存の名前（symlink を含む）があれば失敗する。ユーザーがディレクトリ
    /// を差し替えていても、root がユーザー指定のパスを書き換えることはない。
    fn prepare_sign_files(
        &self,
        challenge: &[u8],
        dir: &tempfile::TempDir,
    ) -> Result<SignFiles, CardError> {
        let dir_file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.path())?;

        let mut challenge_file = open_new_at(&dir_file, c"challenge", libc::O_WRONLY, 0o400)?;
        let signature_file = open_new_at(&dir_file, c"challenge.sig", libc::O_RDWR, 0o600)?;

        challenge_file.write_all(challenge)?;
        let challenge_mode = if self.identity.is_some() {
            // ユーザー側の gpg は /proc/self/fd 経由で読む。
            0o444
        } else {
            0o400
        };
        challenge_file.set_permissions(fs::Permissions::from_mode(challenge_mode))?;

        if let Some(identity) = &self.identity {
            let fd = signature_file.as_raw_fd();
            // SAFETY: fd は開いている signature_file が所有している。
            if unsafe { libc::fchown(fd, identity.uid, identity.gid) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        signature_file.set_permissions(fs::Permissions::from_mode(0o600))?;

        Ok(SignFiles {
            challenge: challenge_file,
            signature: signature_file,
        })
    }

    /// 設定鍵の keygrip を公開鍵から求める。
    fn keygrips(&self) -> Result<Vec<String>, CardError> {
        let mut command = self.command();
        command
            .args([
                "--batch",
                "--no-tty",
                "--with-colons",
                "--with-keygrip",
                "--list-keys",
            ])
            .arg(&self.key);

        let output = match run_command(command, None, self.timeout) {
            Ok(output) => output,
            Err(RunError::Timeout) => return Err(CardError::Timeout),
            Err(RunError::Io(err)) => return Err(CardError::Unavailable(err.to_string())),
        };
        if !output.status.success() {
            return Err(CardError::Unavailable(last_line_or(
                &output.stderr,
                "gpg が鍵を列挙できなかった",
            )));
        }

        let grips = parse_keygrips(&String::from_utf8_lossy(&output.stdout));
        if grips.is_empty() {
            return Err(CardError::Unavailable(
                "設定鍵の keygrip を取得できない".to_string(),
            ));
        }
        Ok(grips)
    }

    /// gpg-agent のパスフレーズキャッシュを署名前に消す。
    ///
    /// gpg-agent は一度成功した PIN をキャッシュし、キャッシュが温まっている間は
    /// 間違った PIN でも署名を通す。PIN を第二要素として扱うため、署名のたびに
    /// 対象鍵のキャッシュを無効化する。
    fn clear_cached_passphrase(&self) -> Result<(), CardError> {
        let program = self.program.with_file_name("gpg-connect-agent");
        for grip in self.keygrips()? {
            let mut command = self.base_command(&program);
            command
                .arg("--no-history")
                .arg(format!("CLEAR_PASSPHRASE --mode=normal {grip}"))
                .arg("/bye");

            let output = match run_command(command, None, self.timeout) {
                Ok(output) => output,
                Err(RunError::Timeout) => return Err(CardError::Timeout),
                Err(RunError::Io(err)) => return Err(CardError::Unavailable(err.to_string())),
            };
            if !output.status.success() {
                return Err(CardError::Unavailable(last_line_or(
                    &output.stderr,
                    "gpg-agent のキャッシュを消せなかった",
                )));
            }
            // gpg-connect-agent はエージェントが ERR を返しても終了コード 0 に
            // なる。終了コードだけを見ると、キャッシュが残ったまま署名へ進み、
            // 温まったキャッシュの PIN で解除できてしまう。
            if !agent_command_succeeded(&output.stdout) {
                return Err(CardError::Unavailable(
                    "gpg-agent が CLEAR_PASSPHRASE を拒否した".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// 公開鍵のバイト列を `homedir` に取り込む。
    ///
    /// 呼び出し側が安全に読んだバイト列を渡せるよう、パスではなく中身を受け取る。
    pub fn import_key_bytes(&self, bytes: &[u8]) -> Result<(), CardError> {
        let homedir = self
            .homedir
            .as_ref()
            .ok_or_else(|| CardError::Unavailable("homedir が無い".to_string()))?;
        let path = homedir.join("nekonolock-pubkey.asc");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(bytes)?;
        drop(file);
        self.import_key(&path)
    }

    /// 信頼した公開鍵を `homedir` に取り込む。root 側の検証用。
    pub fn import_key(&self, path: &Path) -> Result<(), CardError> {
        let mut command = self.command();
        command.args(["--batch", "--no-tty", "--import"]).arg(path);

        let output = match run_command(command, None, self.timeout) {
            Ok(output) => output,
            Err(RunError::Timeout) => return Err(CardError::Timeout),
            Err(RunError::Io(err)) => return Err(CardError::Unavailable(err.to_string())),
        };
        if !output.status.success() {
            return Err(CardError::Unavailable(last_line_or(
                &output.stderr,
                "gpg が公開鍵を取り込めなかった",
            )));
        }
        Ok(())
    }
}

/// 子プロセスの実 uid / 実効 uid を `uid` / `gid` に切り替える。
///
/// `pre_exec` は fork 後・exec 前に走る。ここでは async-signal-safe な
/// syscall だけを使う。補助グループは引き継がず、umask も絞る。
#[allow(unsafe_code)]
fn switch_identity(command: &mut Command, uid: u32, gid: u32) {
    // SAFETY: pre_exec の契約どおり、子プロセスで単純な syscall だけを行う。
    unsafe {
        command.pre_exec(move || {
            if libc::setgroups(0, std::ptr::null()) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setgid(gid) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setuid(uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::umask(0o077);
            Ok(())
        });
    }
}

/// `dir` からの相対パスで新しい通常ファイルを作る。
///
/// `O_EXCL` により既存の名前（symlink を含む）には触れない。`O_NOFOLLOW` は
/// 最終コンポーネントの symlink を辿らないための二重の防御。
#[allow(unsafe_code)]
fn open_new_at(
    dir: &fs::File,
    name: &CStr,
    access: i32,
    mode: libc::mode_t,
) -> Result<fs::File, CardError> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            access | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: openat が返した新しい fd の所有権を受け取る。
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

/// 子プロセスで `file` を `target` の fd 番号へ複製する。
///
/// `/proc/self/fd/<target>` を gpg に渡すため、exec 後も残す。
#[allow(unsafe_code)]
fn inherit_fd(command: &mut Command, file: &fs::File, target: RawFd) {
    let source = file.as_raw_fd();
    // SAFETY: pre_exec の契約どおり、子プロセスで dup2 だけを行う。
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source, target) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

impl CardSigner for GpgCard {
    fn present(&self) -> bool {
        let mut command = self.command();
        command.args(["--batch", "--no-tty", "--card-status"]);

        match run_command(command, None, self.timeout.min(Duration::from_secs(10))) {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                !self.key.is_empty() && key_matches(&text, &self.key)
            }
            _ => false,
        }
    }

    fn sign(&self, challenge: &[u8], pin: &str) -> Result<Vec<u8>, CardError> {
        self.clear_cached_passphrase()?;

        let dir = self.private_tempdir("nekonolock-sign-")?;
        let files = self.prepare_sign_files(challenge, &dir)?;

        let mut command = self.command();
        inherit_fd(&mut command, &files.challenge, CHALLENGE_FD);
        inherit_fd(&mut command, &files.signature, SIGNATURE_FD);
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
            .arg(format!("/proc/self/fd/{SIGNATURE_FD}"))
            .arg(format!("/proc/self/fd/{CHALLENGE_FD}"));

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

        // 署名はユーザーが差し替えられるパスからではなく、開いた fd から読む。
        let mut signature_file = files.signature;
        signature_file.seek(SeekFrom::Start(0))?;
        let mut signature = Vec::new();
        signature_file.read_to_end(&mut signature)?;
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
        if has_unusable_key(&status) {
            return Ok(false);
        }
        match parse_validsig(&status) {
            Some(fingerprint) => Ok(fingerprint_matches(fingerprint, &self.key)),
            None => Ok(false),
        }
    }
}

fn normalize_key(key: &str) -> String {
    key.trim().trim_end_matches('!').to_string()
}

/// gpg-connect-agent の標準出力がコマンド成功を示すか。
///
/// エージェントがコマンドを拒否しても gpg-connect-agent は終了コード 0 を
/// 返すため、`OK` が 1 つ以上あり `ERR` が無いことを確認する。
fn agent_command_succeeded(stdout: &[u8]) -> bool {
    let mut saw_ok = false;
    for line in String::from_utf8_lossy(stdout).lines() {
        let line = line.trim();
        if line.starts_with("ERR") {
            return false;
        }
        if line == "OK" {
            saw_ok = true;
        }
    }
    saw_ok
}

/// `gpg --with-colons --with-keygrip --list-keys` の出力から keygrip を取り出す。
fn parse_keygrips(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("grp:"))
        .filter_map(|rest| rest.split(':').nth(8))
        .map(str::trim)
        .filter(|grip| !grip.is_empty())
        .map(str::to_string)
        .collect()
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

/// 署名に使われた fingerprint と設定鍵を比べる。
///
/// 設定が 16 桁の long key ID なら fpr の末尾一致、40 桁の fingerprint なら
/// 完全一致にする。短い鍵指定や部分一致で別鍵を受け入れない。
fn fingerprint_matches(fingerprint: &str, key: &str) -> bool {
    let key = normalize_key(key).to_ascii_uppercase();
    if key.is_empty() {
        return false;
    }
    let fingerprint: String = fingerprint
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    if key.len() == 40 {
        fingerprint == key
    } else {
        fingerprint.len() >= key.len() && fingerprint.ends_with(&key)
    }
}

/// 失効・期限切れの鍵による署名を検出する。
///
/// カードは鍵の失効を知らないため、署名自体は成功する。検証側で
/// `REVKEYSIG` / `KEYREVOKED` などを止めないと、失効済みの鍵で解除できてしまう。
fn has_unusable_key(status_output: &str) -> bool {
    status_output.lines().any(|line| {
        let Some(rest) = line.strip_prefix("[GNUPG:] ") else {
            return false;
        };
        matches!(
            rest.split_whitespace().next(),
            Some("REVKEYSIG" | "EXPKEYSIG" | "KEYREVOKED" | "KEYEXPIRED" | "SIGEXPIRED")
        )
    })
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
        || lower.contains("bad passphrase")
        || lower.contains("pin blocked")
        || lower.contains("card is blocked")
    {
        return CardError::BadPin;
    }
    // カードの詳細や鍵の情報を返さないよう、最後の 1 行だけにする。
    CardError::Unavailable(last_line_or(stderr.as_bytes(), "gpg が失敗した"))
}

/// stderr の最後の非空行だけを返す。詳細をエラーに載せすぎない。
fn last_line_or(stderr: &[u8], fallback: &str) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| fallback.to_string())
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
            classify_sign_failure("gpg: signing failed: Bad passphrase"),
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
    fn fingerprint_matching_is_strict_for_key_ids() {
        let fpr = "3B1F00001111222233334444030130E2F156CD74";
        assert!(fingerprint_matches(fpr, "030130E2F156CD74"));
        assert!(fingerprint_matches(
            fpr,
            "3B1F00001111222233334444030130E2F156CD74"
        ));
        assert!(!fingerprint_matches(fpr, "0000111122223333"));
        assert!(!fingerprint_matches(
            "FFFF00001111222233334444030130E2F156CD74",
            "0000111122223333"
        ));
        assert!(!fingerprint_matches(fpr, "!"));
    }

    #[test]
    fn detects_revoked_and_expired_status() {
        assert!(has_unusable_key("[GNUPG:] REVKEYSIG 1234 audit"));
        assert!(has_unusable_key("[GNUPG:] EXPKEYSIG 1234 audit"));
        assert!(has_unusable_key("[GNUPG:] KEYREVOKED"));
        assert!(has_unusable_key("[GNUPG:] KEYEXPIRED 123"));
        assert!(has_unusable_key("[GNUPG:] SIGEXPIRED"));
        assert!(!has_unusable_key(
            "[GNUPG:] GOODSIG 1234 audit\n[GNUPG:] VALIDSIG 030130E2F156CD74 2026-11-15"
        ));
    }

    #[test]
    fn normalize_key_strips_suffix() {
        assert_eq!(normalize_key(" ABCD! "), "ABCD");
        assert_eq!(normalize_key(""), "");
    }

    #[test]
    fn agent_command_requires_ok_and_rejects_err() {
        assert!(agent_command_succeeded(b"OK\n"));
        assert!(agent_command_succeeded(b" OK \r\n"));
        assert!(!agent_command_succeeded(b""));
        assert!(!agent_command_succeeded(
            b"ERR 67109144 IPC parameter error <GPG Agent>\n"
        ));
        assert!(!agent_command_succeeded(b"OK\nERR 67109144 rejected\n"));
    }

    #[test]
    fn prepare_sign_files_refuses_an_existing_symlink() {
        let dir = tempfile::Builder::new()
            .prefix("nekonolock-test-")
            .tempdir()
            .unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"UNCHANGED").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("challenge")).unwrap();

        let card = GpgCard::new("A".repeat(40), Duration::from_secs(1));
        let err = card.prepare_sign_files(b"challenge", &dir).unwrap_err();
        assert!(matches!(err, CardError::Io(_)), "unexpected: {err:?}");
        assert_eq!(fs::read(&target).unwrap(), b"UNCHANGED");
    }

    #[test]
    fn sign_reads_the_signature_from_the_open_descriptor() {
        let dir = tempfile::Builder::new()
            .prefix("nekonolock-gpg-test-")
            .tempdir()
            .unwrap();
        let gpg = dir.path().join("gpg");
        let agent = dir.path().join("gpg-connect-agent");
        fs::write(
            &gpg,
            "\
#!/bin/sh
case \" $* \" in
  *\" --list-keys \"*)
    echo 'grp:::::::::AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
    exit 0
    ;;
  *\" --detach-sign \"*)
    prev=\"\"
    out=\"\"
    for arg in \"$@\"; do
      if [ \"$prev\" = \"--output\" ]; then out=\"$arg\"; fi
      prev=\"$arg\"
    done
    printf 'signature' > \"$out\"
    exit 0
    ;;
esac
exit 0
",
        )
        .unwrap();
        fs::write(&agent, "#!/bin/sh\necho OK\nexit 0\n").unwrap();
        fs::set_permissions(&gpg, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();

        let card = GpgCard::new("A".repeat(40), Duration::from_secs(5)).with_program(&gpg);
        let signature = card.sign(b"challenge", "123456").unwrap();
        assert_eq!(signature, b"signature");
    }

    #[test]
    fn parses_keygrips_from_colons_output() {
        let output = "\
pub:-:255:22:8169485A377499E4:1794735933:1794822333::-:::scSC:::::ed25519:::0:
fpr:::::::::E03DF3B6B0F8777456708F568169485A377499E4:
grp:::::::::9195E7B9B80CA0AD04F0859DD4C7F932E9AD3464:
sub:-:255:22:1111111111111111:1794735933:1794822333:::::s:::::ed25519:::0:
fpr:::::::::AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:
grp:::::::::BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB:
";
        assert_eq!(
            parse_keygrips(output),
            [
                "9195E7B9B80CA0AD04F0859DD4C7F932E9AD3464",
                "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            ]
        );
        assert!(parse_keygrips("pub:-:255:22:ABCD::::\n").is_empty());
    }
}
