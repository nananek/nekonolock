//! 1 接続 1 要求のサーバループ。

use std::time::Duration;

use anyhow::Result;
use nekonolock_proto::{
    Connection, Listener, PROTOCOL_VERSION, Reason, Request, Response, ResponseBody, SocketError,
};
use zeroize::Zeroize;

use crate::card::CardSigner;
use crate::engine::Engine;
use crate::pam::PasswordAuthenticator;

/// `once` が true なら 1 接続を処理して終了する（テストと手動確認用）。
pub fn serve<C: CardSigner, P: PasswordAuthenticator>(
    listener: &Listener,
    engine: &Engine<C, P>,
    timeout: Duration,
    once: bool,
) -> Result<()> {
    loop {
        let connection = match listener.accept_allowed() {
            Ok(connection) => connection,
            Err(err) => {
                eprintln!("nekonolock-auth: accept に失敗した: {err}");
                return Err(err.into());
            }
        };

        handle_connection(&connection, engine, timeout);

        if once {
            break;
        }
    }
    Ok(())
}

fn handle_connection<C: CardSigner, P: PasswordAuthenticator>(
    connection: &Connection,
    engine: &Engine<C, P>,
    timeout: Duration,
) {
    let mut bytes = match connection.read_message(timeout) {
        Ok(bytes) => bytes,
        Err(SocketError::Timeout) => {
            // 応答しない。UI は切断を「ヘルパに接続できない」として扱い、ロックを維持する。
            eprintln!("nekonolock-auth: 要求がタイムアウトした");
            return;
        }
        Err(err) => {
            eprintln!("nekonolock-auth: 要求を読めない: {err}");
            return;
        }
    };

    let response = match serde_json::from_slice::<Request>(&bytes) {
        Ok(request) if request.v == PROTOCOL_VERSION => engine.handle(&request),
        Ok(_) => Response::failed(Reason::BadRequest),
        Err(_) => Response::failed(Reason::BadRequest),
    };
    // 受信した JSON には秘密が含まれる。応答を作ったらすぐ消す。
    bytes.zeroize();

    log_response(&response);

    match serde_json::to_vec(&response) {
        Ok(json) => {
            if let Err(err) = connection.write_message(&json) {
                eprintln!("nekonolock-auth: 応答を書けない: {err}");
            }
        }
        Err(err) => eprintln!("nekonolock-auth: 応答を JSON にできない: {err}"),
    }
}

/// 秘密を出さずに結果だけをログへ書く。
fn log_response(response: &Response) {
    match &response.body {
        ResponseBody::Status { card, methods, .. } => {
            eprintln!(
                "nekonolock-auth: status card={card} methods={}",
                methods
                    .iter()
                    .map(|method| method.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        ResponseBody::Unlock { method, .. } => {
            eprintln!("nekonolock-auth: unlocked method={}", method.as_str());
        }
        ResponseBody::Error { reason, .. } => {
            eprintln!("nekonolock-auth: failed reason={}", reason.as_str());
        }
    }
}
