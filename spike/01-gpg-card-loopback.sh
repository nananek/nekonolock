#!/bin/bash
# Phase 1 spike: カード上の秘密鍵に loopback pinentry で署名できるか。
#
# 目的: nekonolock-auth が PIN を「pinentry のダイアログを出さずに」カードへ
#       渡せるかを確認する。成功すれば設計どおり実装できる。失敗する場合は
#       gpg-agent へ Assuan で直接 SIGKEY/PWD を送る方式へ切り替える。
#
# 使い方:
#   bash spike/01-gpg-card-loopback.sh [KEY-FPR]
#   （YubiKey を挿し、PIN を入力する。既定 FPR は ayaka の primary。）
#
# PIN は argv に出さない。stdin 経由で gpg に渡す。
set -euo pipefail

FPR="${1:-030130E2F156CD74}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

head -c 32 /dev/urandom > "$tmp/challenge"

echo "key: ${FPR}!"
echo "challenge: $tmp/challenge (32 bytes)"
read -r -s -p "PIN: " pin
echo

if ! printf '%s' "$pin" | gpg --batch --yes --local-user "${FPR}!" \
    --detach-sign --pinentry-mode loopback --passphrase-fd 0 \
    --output "$tmp/challenge.sig" "$tmp/challenge" 2>"$tmp/err"; then
    echo "FAIL: sign"
    sed -E 's/^(gpg: )?.*[Pp][Ii][Nn].*/[pin-related line masked]/' "$tmp/err" | tail -5
    exit 1
fi

echo "sign: OK"
if gpg --verify "$tmp/challenge.sig" "$tmp/challenge" 2>&1 | tail -3; then
    echo "verify: OK"
else
    echo "verify: FAIL"
    exit 2
fi
