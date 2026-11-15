//! Linux-PAM のモジュール API と会話関数。
//!
//! ここだけが PAM の FFI。秘密（PIN）は戻り値の `Zeroizing<String>` に閉じ込め、
//! ログには出さない。

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use zeroize::Zeroizing;

pub const PAM_SUCCESS: c_int = 0;
pub const PAM_IGNORE: c_int = 25;

pub const PAM_CONV: c_int = 5;

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

const LOG_WARNING: c_int = 4;
const LOG_ERR: c_int = 3;
const LOG_INFO: c_int = 6;

#[repr(C)]
pub struct PamMessage {
    pub msg_style: c_int,
    pub msg: *const c_char,
}

#[repr(C)]
pub struct PamResponse {
    pub resp: *mut c_char,
    pub resp_retcode: c_int,
}

pub type Conversation = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int;

#[repr(C)]
pub struct PamConv {
    pub conv: Option<Conversation>,
    pub appdata_ptr: *mut c_void,
}

type PamGetItem =
    unsafe extern "C" fn(pamh: *const c_void, item_type: c_int, item: *mut *const c_void) -> c_int;

#[derive(Debug, thiserror::Error)]
pub enum PamError {
    #[error("libpam を開けない: {0}")]
    Load(String),
    #[error("PAM の会話関数を取得できない")]
    NoConversation,
    #[error("PAM の会話に失敗した (code {0})")]
    Conversation(c_int),
    #[error("PAM の応答が UTF-8 ではない")]
    BadResponse,
}

/// PAM ハンドルから会話構造体を得る。
///
/// SAFETY: `pamh` は libpam がモジュールに渡した有効なハンドルであること。
unsafe fn conversation(pamh: *const c_void) -> Result<*const PamConv, PamError> {
    // モジュール自身は libpam にリンクしない。PAM アプリが既に読み込んでいる
    // `libpam.so.0` から pam_get_item を取る（ビルドに PAM の開発用
    // パッケージを要求しない。認証ヘルパの dlopen と同じ方針）。
    let library = unsafe { libloading::Library::new("libpam.so.0") }
        .map_err(|err| PamError::Load(err.to_string()))?;
    let pam_get_item: libloading::Symbol<'_, PamGetItem> = unsafe {
        library
            .get(b"pam_get_item\0")
            .map_err(|err| PamError::Load(format!("pam_get_item が無い: {err}")))?
    };

    let mut item: *const c_void = ptr::null();
    // SAFETY: pamh は呼び出し側の契約どおり。item は書き込み可能。
    let rc = unsafe { pam_get_item(pamh, PAM_CONV, &mut item) };
    if rc != PAM_SUCCESS || item.is_null() {
        return Err(PamError::NoConversation);
    }
    Ok(item.cast())
}

/// エコーなしのプロンプトを出し、入力を受け取る。キャンセルは `None`。
///
/// SAFETY: `pamh` は libpam が渡した有効なハンドルであること。
pub unsafe fn ask(
    pamh: *const c_void,
    prompt: &str,
) -> Result<Option<Zeroizing<String>>, PamError> {
    let prompt = CString::new(prompt).map_err(|_| PamError::BadResponse)?;
    let messages = [PamMessage {
        msg_style: PAM_PROMPT_ECHO_OFF,
        msg: prompt.as_ptr(),
    }];

    let conv = unsafe { conversation(pamh)? };
    // SAFETY: conv は libpam が保持する有効な会話構造体。
    let Some(callback) = (unsafe { (*conv).conv }) else {
        return Err(PamError::NoConversation);
    };
    let appdata = unsafe { (*conv).appdata_ptr };

    let mut message_ptr = messages.as_ptr();
    let mut response: *mut PamResponse = ptr::null_mut();
    // SAFETY: PAM の会話関数の契約（1 メッセージ、応答配列を返す）。
    let rc = unsafe { callback(1, &mut message_ptr, &mut response, appdata) };
    if response.is_null() {
        return if rc == PAM_SUCCESS {
            Ok(None)
        } else {
            Err(PamError::Conversation(rc))
        };
    }

    // SAFETY: response は 1 要素の配列。resp は PAM が strdup した文字列。
    let raw = unsafe { (*response).resp };
    let result = if raw.is_null() {
        Ok(None)
    } else {
        // SAFETY: raw は NUL 終端文字列。
        let bytes = unsafe { CStr::from_ptr(raw) }.to_bytes();
        match String::from_utf8(bytes.to_vec()) {
            // 応答は PAM が確保した領域。コピー後に free する。
            Ok(text) => Ok(Some(Zeroizing::new(text))),
            Err(_) => Err(PamError::BadResponse),
        }
    };

    unsafe {
        if !raw.is_null() {
            libc::free(raw.cast());
        }
        libc::free(response.cast());
    }
    if rc != PAM_SUCCESS {
        return Err(PamError::Conversation(rc));
    }
    result
}

/// エラー / 情報メッセージを出す。失敗しても認証結果は変えない。
///
/// SAFETY: `pamh` は libpam が渡した有効なハンドルであること。
pub unsafe fn message(pamh: *const c_void, text: &str, error: bool) {
    let Ok(text) = CString::new(text) else {
        return;
    };
    let Ok(conv) = (unsafe { conversation(pamh) }) else {
        return;
    };
    // SAFETY: conv は libpam が保持する有効な会話構造体。
    let Some(callback) = (unsafe { (*conv).conv }) else {
        return;
    };
    let messages = [PamMessage {
        msg_style: if error { PAM_ERROR_MSG } else { PAM_TEXT_INFO },
        msg: text.as_ptr(),
    }];
    let appdata = unsafe { (*conv).appdata_ptr };
    let mut message_ptr = messages.as_ptr();
    let mut response: *mut PamResponse = ptr::null_mut();
    // SAFETY: PAM の会話関数の契約。
    let _ = unsafe { callback(1, &mut message_ptr, &mut response, appdata) };
    if !response.is_null() {
        // SAFETY: 応答配列は PAM の会話関数が確保したもの。テキストメッセージ
        // に対する応答は通常 null だが、返ってきた場合は解放する。
        unsafe {
            let raw = (*response).resp;
            if !raw.is_null() {
                libc::free(raw.cast());
            }
            libc::free(response.cast());
        }
    }
}

/// syslog に理由だけを書く。PIN / パスワードは渡さない。
pub fn log(level: LogLevel, text: &str) {
    let text = text.replace('\0', " ");
    let Ok(text) = CString::new(text) else {
        return;
    };
    let priority = match level {
        LogLevel::Info => LOG_INFO,
        LogLevel::Warning => LOG_WARNING,
        LogLevel::Error => LOG_ERR,
    };
    // SAFETY: ident と format は静的な NUL 終端文字列。text は有効。
    unsafe {
        libc::openlog(
            c"pam_nekonolock".as_ptr(),
            libc::LOG_PID,
            libc::LOG_AUTHPRIV,
        );
        libc::syslog(priority | libc::LOG_AUTHPRIV, c"%s".as_ptr(), text.as_ptr());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_linux_pam() {
        assert_eq!(PAM_SUCCESS, 0);
        assert_eq!(PAM_IGNORE, 25);
        assert_eq!(PAM_CONV, 5);
    }
}
