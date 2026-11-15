//! 実 uid から、署名に使うユーザーの情報を引く。
//!
//! PAM モジュールは sudo から root で呼ばれる。`PAM_USER` は sudo の
//! 対象ユーザー（通常 root）なので使わず、`getuid()` の実 uid を使う。
//! 環境変数（`HOME` / `XDG_RUNTIME_DIR`）も sudo が捨てる可能性があるため、
//! passwd と `/run/user/<uid>` から組み立てる。

use std::ffi::{CStr, c_char};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::ptr;

use nekonolock_gpg::Identity;

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("uid {0} のユーザーが /etc/passwd に無い")]
    NotFound(u32),
    #[error("ユーザー情報を読めない: {0}")]
    Lookup(String),
    #[error("ユーザーの home が安全でない: {0}")]
    UnsafeHome(PathBuf),
    #[error("XDG_RUNTIME_DIR が安全でない: {0}")]
    UnsafeRuntime(PathBuf),
}

/// 実 uid のユーザーを引く。root は呼ばない前提。
pub fn lookup(uid: u32) -> Result<Identity, UserError> {
    let entry = passwd(uid)?;
    let home = entry.home;
    let runtime = PathBuf::from(format!("/run/user/{uid}"));

    let home_meta =
        std::fs::symlink_metadata(&home).map_err(|_| UserError::UnsafeHome(home.clone()))?;
    if !home_meta.is_dir() || home_meta.uid() != uid || home_meta.permissions().mode() & 0o022 != 0
    {
        return Err(UserError::UnsafeHome(home));
    }

    let runtime_meta = std::fs::symlink_metadata(&runtime)
        .map_err(|_| UserError::UnsafeRuntime(runtime.clone()))?;
    if !runtime_meta.is_dir()
        || runtime_meta.uid() != uid
        || runtime_meta.permissions().mode() & 0o077 != 0
    {
        return Err(UserError::UnsafeRuntime(runtime));
    }

    Ok(Identity {
        uid,
        gid: entry.gid,
        name: entry.name,
        home,
        runtime_dir: runtime,
    })
}

struct PasswdEntry {
    name: String,
    gid: u32,
    home: PathBuf,
}

fn passwd(uid: u32) -> Result<PasswdEntry, UserError> {
    let mut size = initial_buffer_size();
    loop {
        let mut buf = vec![0 as c_char; size];
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = ptr::null_mut();

        // SAFETY: buf は size byte の書き込み可能な領域、entry は有効な
        // passwd 構造体。getpwuid_r は result を buf 内のポインタに設定する。
        let rc =
            unsafe { libc::getpwuid_r(uid, &mut entry, buf.as_mut_ptr(), buf.len(), &mut result) };

        if rc == libc::ERANGE {
            if size >= 1024 * 1024 {
                return Err(UserError::Lookup("passwd のバッファが足りない".to_string()));
            }
            size *= 2;
            continue;
        }
        if rc != 0 {
            return Err(UserError::Lookup(format!(
                "getpwuid_r が失敗した (code {rc})"
            )));
        }
        if result.is_null() {
            return Err(UserError::NotFound(uid));
        }

        // SAFETY: getpwuid_r が成功したので、各フィールドは buf 内の
        // NUL 終端文字列を指している。buf はここでまだ生きている。
        let name = unsafe { c_string(entry.pw_name)? };
        let home = unsafe { c_string(entry.pw_dir)? };
        if name.is_empty() || home.is_empty() {
            return Err(UserError::Lookup("passwd の名前か home が空".to_string()));
        }

        return Ok(PasswdEntry {
            name,
            gid: entry.pw_gid,
            home: PathBuf::from(home),
        });
    }
}

fn initial_buffer_size() -> usize {
    // SAFETY: sysconf は単純な問い合わせ。負値や 0 は既定値に落とす。
    let size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    if size > 0 {
        (size as usize).clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    }
}

/// SAFETY: `ptr` は有効な NUL 終端 C 文字列を指していること。
unsafe fn c_string(ptr: *const c_char) -> Result<String, UserError> {
    if ptr.is_null() {
        return Err(UserError::Lookup("passwd に null がある".to_string()));
    }
    // SAFETY: 呼び出し側の契約。
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    String::from_utf8(bytes.to_vec())
        .map_err(|_| UserError::Lookup("passwd が UTF-8 ではない".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_has_a_runtime_dir_or_fails_closed() {
        // テスト環境では /run/user/<uid> が無いこともある。どちらでも
        // パニックせず、結果を返すことを確認する。
        match lookup(nix_uid()) {
            Ok(identity) => {
                assert_eq!(identity.uid, nix_uid());
                assert!(!identity.name.is_empty());
            }
            Err(UserError::UnsafeRuntime(_)) | Err(UserError::UnsafeHome(_)) => {}
            Err(other) => panic!("unexpected: {other}"),
        }
    }

    fn nix_uid() -> u32 {
        // SAFETY: getuid は引数を取らない。
        unsafe { libc::getuid() }
    }
}
