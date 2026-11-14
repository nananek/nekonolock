//! PAM によるパスワード認証。
//!
//! `pam-client` はビルドに clang / libpam のヘッダを要求するため使わず、
//! `libpam.so.0` を実行時に `dlopen` する。これで CI と配布物のビルドに
//! PAM の開発用パッケージが不要になる。

use std::ffi::{CString, c_char, c_int, c_void};
use std::mem::size_of;
use std::ptr;

use libloading::{Library, Symbol};
use zeroize::Zeroizing;

pub trait PasswordAuthenticator {
    fn authenticate(&self, user: &str, password: &str) -> Result<bool, PamError>;
}

#[derive(Debug, thiserror::Error)]
pub enum PamError {
    #[error("libpam を開けない: {0}")]
    Load(String),
    #[error("PAM の呼び出しに失敗した: {0}")]
    Call(String),
}

/// `/etc/pam.d/<service>` を使う実装。
#[derive(Debug, Clone)]
pub struct PamAuth {
    service: String,
}

impl PamAuth {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

type PamHandle = c_void;
type PamStart = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *const PamConv,
    *mut *mut PamHandle,
) -> c_int;
type PamAuthenticate = unsafe extern "C" fn(*mut PamHandle, c_int) -> c_int;
type PamAcctMgmt = unsafe extern "C" fn(*mut PamHandle, c_int) -> c_int;
type PamEnd = unsafe extern "C" fn(*mut PamHandle, c_int) -> c_int;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: Option<
        unsafe extern "C" fn(
            c_int,
            *mut *const PamMessage,
            *mut *mut PamResponse,
            *mut c_void,
        ) -> c_int,
    >,
    appdata_ptr: *mut c_void,
}

/// 会話コールバックに渡す入力。パスワードは NUL 終端したバッファを指す。
struct ConvContext {
    username: *const c_char,
    password: *const c_char,
}

const PAM_SUCCESS: c_int = 0;
const PAM_SYSTEM_ERR: c_int = 4;
const PAM_BUF_ERR: c_int = 5;
const PAM_PERM_DENIED: c_int = 6;
const PAM_AUTH_ERR: c_int = 7;
const PAM_CRED_INSUFFICIENT: c_int = 8;
const PAM_AUTHINFO_UNAVAIL: c_int = 9;
const PAM_USER_UNKNOWN: c_int = 10;
const PAM_MAXTRIES: c_int = 11;
const PAM_NEW_AUTHTOK_REQD: c_int = 12;
const PAM_ACCT_EXPIRED: c_int = 13;
const PAM_CRED_UNAVAIL: c_int = 15;
const PAM_CRED_EXPIRED: c_int = 16;
const PAM_CRED_ERR: c_int = 17;
const PAM_CONV_ERR: c_int = 19;
const PAM_ABORT: c_int = 26;

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

impl PasswordAuthenticator for PamAuth {
    fn authenticate(&self, user: &str, password: &str) -> Result<bool, PamError> {
        if password.as_bytes().contains(&0) {
            return Err(PamError::Call(
                "パスワードに NUL を含められない".to_string(),
            ));
        }

        let service = CString::new(self.service.as_bytes())
            .map_err(|_| PamError::Call("service 名が不正".to_string()))?;
        let username = CString::new(user.as_bytes())
            .map_err(|_| PamError::Call("ユーザー名が不正".to_string()))?;
        let mut password_buf = Zeroizing::new(password.as_bytes().to_vec());
        password_buf.push(0);

        let context = ConvContext {
            username: username.as_ptr(),
            password: password_buf.as_ptr().cast(),
        };
        let conversation = PamConv {
            conv: Some(conversation),
            appdata_ptr: (&context as *const ConvContext).cast_mut().cast(),
        };

        // SAFETY: すべて PAM の公開 API に沿った呼び出し。handle は pam_start が
        // 成功したときだけ pam_end に渡す。context / password_buf は関数末尾まで生きる。
        let library = unsafe { Library::new("libpam.so.0") }
            .map_err(|err| PamError::Load(err.to_string()))?;

        let pam_start: Symbol<PamStart> = unsafe { library.get(b"pam_start\0") }
            .map_err(|err| PamError::Load(format!("pam_start が無い: {err}")))?;
        let pam_authenticate: Symbol<PamAuthenticate> =
            unsafe { library.get(b"pam_authenticate\0") }
                .map_err(|err| PamError::Load(format!("pam_authenticate が無い: {err}")))?;
        let pam_acct_mgmt: Symbol<PamAcctMgmt> = unsafe { library.get(b"pam_acct_mgmt\0") }
            .map_err(|err| PamError::Load(format!("pam_acct_mgmt が無い: {err}")))?;
        let pam_end: Symbol<PamEnd> = unsafe { library.get(b"pam_end\0") }
            .map_err(|err| PamError::Load(format!("pam_end が無い: {err}")))?;

        let mut handle: *mut PamHandle = ptr::null_mut();
        // SAFETY: service / username / conversation はこのスコープで有効。
        let start_result = unsafe {
            pam_start(
                service.as_ptr(),
                username.as_ptr(),
                &conversation,
                &mut handle,
            )
        };
        if start_result != PAM_SUCCESS || handle.is_null() {
            return Err(PamError::Call(format!(
                "pam_start が失敗した (code {start_result})"
            )));
        }

        // SAFETY: handle は pam_start が返した有効なハンドル。
        let auth_result = unsafe { pam_authenticate(handle, 0) };
        let result = if auth_result == PAM_SUCCESS {
            // SAFETY: 同上。アカウントの有効期限・ロックも確認する。
            let acct_result = unsafe { pam_acct_mgmt(handle, 0) };
            classify(acct_result)
        } else {
            classify(auth_result)
        };

        // SAFETY: handle は有効で、pam_end の後に使わない。秘密は Zeroizing が消す。
        unsafe {
            pam_end(handle, auth_result);
        }

        result
    }
}

fn classify(code: c_int) -> Result<bool, PamError> {
    match code {
        PAM_SUCCESS => Ok(true),
        PAM_AUTH_ERR
        | PAM_CRED_INSUFFICIENT
        | PAM_AUTHINFO_UNAVAIL
        | PAM_USER_UNKNOWN
        | PAM_MAXTRIES
        | PAM_PERM_DENIED
        | PAM_NEW_AUTHTOK_REQD
        | PAM_ACCT_EXPIRED
        | PAM_CRED_UNAVAIL
        | PAM_CRED_EXPIRED
        | PAM_CRED_ERR => Ok(false),
        PAM_SYSTEM_ERR | PAM_BUF_ERR | PAM_CONV_ERR | PAM_ABORT => {
            Err(PamError::Call(format!("PAM が失敗した (code {code})")))
        }
        other => Err(PamError::Call(format!(
            "PAM が未知の code を返した ({other})"
        ))),
    }
}

/// PAM から呼ばれる会話関数。パスワード要求にはパスワードを、ユーザー名要求には
/// ユーザー名を返す。メッセージ表示には何も返さない。
unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }

    let count = num_msg as usize;
    // SAFETY: appdata はこの関数の呼び出し元が ConvContext を指して渡している。
    let context = unsafe { &*(appdata as *const ConvContext) };

    // SAFETY: PAM は `calloc` した配列と `strdup` した文字列を `free` で解放する。
    // ここではその契約どおりにメモリを用意する。
    let array = unsafe { libc::calloc(count, size_of::<PamResponse>()) } as *mut PamResponse;
    if array.is_null() {
        return PAM_BUF_ERR;
    }

    for index in 0..count {
        // SAFETY: msg は num_msg 個の有効なポインタ配列。
        let message = unsafe { *msg.add(index) };
        if message.is_null() {
            unsafe { free_responses(array, count) };
            return PAM_CONV_ERR;
        }

        // SAFETY: message は PAM が用意した有効なポインタ。
        let style = unsafe { (*message).msg_style };
        // SAFETY: array は count 個分を calloc 済み。
        let response = unsafe { &mut *array.add(index) };
        response.resp_retcode = 0;

        let source = match style {
            PAM_PROMPT_ECHO_OFF => context.password,
            PAM_PROMPT_ECHO_ON => context.username,
            PAM_ERROR_MSG | PAM_TEXT_INFO => continue,
            _ => {
                unsafe { free_responses(array, count) };
                return PAM_CONV_ERR;
            }
        };
        if source.is_null() {
            unsafe { free_responses(array, count) };
            return PAM_CONV_ERR;
        }

        // SAFETY: source は NUL 終端した C 文字列。
        response.resp = unsafe { libc::strdup(source) };
        if response.resp.is_null() {
            unsafe { free_responses(array, count) };
            return PAM_BUF_ERR;
        }
    }

    // SAFETY: resp は PAM が用意した有効なポインタ。
    unsafe { *resp = array };
    PAM_SUCCESS
}

/// 会話関数が確保した配列を解放する。`calloc` なので未使用要素は null。
unsafe fn free_responses(array: *mut PamResponse, count: usize) {
    if array.is_null() {
        return;
    }
    for index in 0..count {
        // SAFETY: array は count 個分を calloc 済み。
        let response = unsafe { &mut *array.add(index) };
        if !response.resp.is_null() {
            // SAFETY: resp は strdup が返した領域。
            unsafe { libc::free(response.resp.cast()) };
        }
    }
    // SAFETY: array は calloc が返した領域。
    unsafe { libc::free(array.cast()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_success_and_failures() {
        assert!(classify(PAM_SUCCESS).unwrap());
        for code in [
            PAM_AUTH_ERR,
            PAM_CRED_INSUFFICIENT,
            PAM_AUTHINFO_UNAVAIL,
            PAM_USER_UNKNOWN,
            PAM_MAXTRIES,
            PAM_PERM_DENIED,
            PAM_NEW_AUTHTOK_REQD,
            PAM_ACCT_EXPIRED,
        ] {
            assert!(!classify(code).unwrap(), "code {code}");
        }
        for code in [PAM_SYSTEM_ERR, PAM_BUF_ERR, PAM_CONV_ERR, PAM_ABORT, 99] {
            assert!(classify(code).is_err(), "code {code}");
        }
    }
}
