#!/bin/bash
# Phase 7: sudo 用 PAM モジュールを Docker 内でビルドし、PAM スタックから
# pam_authenticate を呼ぶ。
#
# 実カードは使わない。テストユーザーのソフトウェア GPG 鍵を
# /etc/nekonolock/sudo-key.asc（root 所有）に秘密鍵として置き、PAM モジュールが
# root 専用の一時 homedir に取り込んで署名する経路を通す。ユーザーの
# gpg-agent は使わない。実カードでは公開鍵を置き、root 側の scdaemon が
# カードを開いて署名する。
# 確認すること:
#   1. 設定が無いときは PAM_IGNORE を返し、パスワード側へフォールバックする
#   2. 正しい PIN（鍵のパスフレーズ）なら pam_authenticate が成功する
#   3. ユーザーの gpg-agent が温まっていても、root 専用の一時 agent で
#      間違った PIN を拒否する（PIN 検証をユーザー agent に委ねない回帰テスト）
#
# 前提:
#   - Docker（rootless 可）
#   - 初回は Debian ミラー / crates.io へ接続できるネットワーク
#
# 使い方:
#   bash spike/05-sudo-pam.sh
#
# 環境変数:
#   CA_CERT     追加で信頼する CA 証明書（TLS を検査する環境向け。任意）
#   DOCKER_ARGS docker run に渡す追加引数（例: --net=host）
#   IMAGE       既存イメージを使う（SKIP_BUILD=1 と組で使う）
#   SKIP_BUILD=1 Dockerfile のビルドを省略する
#   KEEP=1      終了時に WORK を残す
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=${WORK:-$(mktemp -d)}
if [ -z "${KEEP:-}" ]; then
    trap 'rm -rf "$work"' EXIT
fi
mkdir -p "$work"
echo "==> work: $work"

if [ -n "${CA_CERT:-}" ]; then
    cp "$CA_CERT" "$work/ca.crt"
else
    : >"$work/ca.crt"
fi

cat >"$work/Dockerfile" <<'EOF'
FROM rust:1.99
COPY ca.crt /tmp/ca.crt
RUN if [ -s /tmp/ca.crt ]; then \
        cp /tmp/ca.crt /usr/local/share/ca-certificates/local-ca.crt \
        && update-ca-certificates; \
    fi \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates gcc gnupg libpam0g-dev sudo \
 && rm -rf /var/lib/apt/lists/*
EOF

image=${IMAGE:-local/nekonolock-pamtest:latest}
if [ -z "${SKIP_BUILD:-}" ]; then
    docker build -q -t "$image" -f "$work/Dockerfile" "$work"
fi

cat >"$work/pamtest.c" <<'EOF'
#include <security/pam_appl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char *secret;

static int conv(int n, const struct pam_message **msg,
                struct pam_response **resp, void *data) {
    (void)msg;
    (void)data;
    struct pam_response *r = calloc((size_t)n, sizeof(*r));
    if (!r) {
        return PAM_BUF_ERR;
    }
    for (int i = 0; i < n; i++) {
        r[i].resp = strdup(secret);
        r[i].resp_retcode = 0;
    }
    *resp = r;
    return PAM_SUCCESS;
}

int main(int argc, char **argv) {
    if (argc < 4) {
        fprintf(stderr, "usage: %s SERVICE USER SECRET\n", argv[0]);
        return 2;
    }
    secret = argv[3];
    struct pam_conv c = {conv, NULL};
    pam_handle_t *h = NULL;
    int rc = pam_start(argv[1], argv[2], &c, &h);
    if (rc != PAM_SUCCESS) {
        printf("pam_start=%d\n", rc);
        return 2;
    }
    rc = pam_authenticate(h, 0);
    printf("pam_authenticate=%d (%s)\n", rc, pam_strerror(h, rc));
    pam_end(h, rc);
    return rc == PAM_SUCCESS ? 0 : 1;
}
EOF

cat >"$work/guest.sh" <<'EOF'
#!/bin/bash
set -uo pipefail
fail=0
note() { echo "phase7: $*"; }
check() { if eval "$2"; then note "ok: $1"; else note "FAIL: $1"; fail=1; fi; }

export CARGO_HOME=/out/cargo-home
export CARGO_TARGET_DIR=/out/target
cd /work
if cargo build --release -p pam-nekonolock >/tmp/build.log 2>&1; then
    note "ok: pam-nekonolock をビルド"
else
    note "FAIL: pam-nekonolock のビルド"
    tail -60 /tmp/build.log
    exit 1
fi

gcc -O2 -o /tmp/pamtest /out/pamtest.c -lpam || exit 1
security_dir=$(dirname "$(find /usr/lib -name pam_permit.so | head -1)")
install -Dm755 "$CARGO_TARGET_DIR/release/libpam_nekonolock.so" \
    "$security_dir/pam_nekonolock.so"

useradd -m -u 1000 tester
install -d -o tester -g tester -m 0700 /run/user/1000

cat >/etc/pam.d/nekonolock-fallback <<'PAM'
auth sufficient pam_nekonolock.so debug
auth required pam_permit.so
account required pam_permit.so
PAM
cat >/etc/pam.d/nekonolock-required <<'PAM'
auth sufficient pam_nekonolock.so debug
auth required pam_deny.so
account required pam_permit.so
PAM

# 1. 設定が無い -> PAM_IGNORE -> pam_permit で成功する。
if su tester -c '/tmp/pamtest nekonolock-fallback tester anything' >/tmp/f1.log 2>&1; then
    note "ok: 設定が無いときはパスワードへフォールバック"
else
    note "FAIL: フォールバック"
    cat /tmp/f1.log
    fail=1
fi

# 2. sudo 経由で本物の root PAM 文脈（実 uid = tester、実効 uid = root）で
#    動かす。直接 pam_authenticate を tester で呼ぶと、署名用の setuid が
#    権限不足で失敗するため。
cat >/etc/pam.d/sudo <<'PAM'
auth sufficient pam_nekonolock.so debug
auth required pam_deny.so
account required pam_permit.so
session required pam_permit.so
PAM
echo 'tester ALL=(ALL) ALL' >/etc/sudoers.d/tester
chmod 0440 /etc/sudoers.d/tester

# 3. ソフトウェア鍵を作り、秘密鍵を信頼した鍵ファイルに置く。
mkdir -p /etc/nekonolock
su tester -c 'gpg --batch --pinentry-mode loopback --passphrase test-password \
    --quick-generate-key "Tester <tester@example>" ed25519 sign 1d' >/tmp/gpg.log 2>&1
fpr=$(su tester -c 'gpg --batch --with-colons --list-keys 2>/dev/null | awk -F: "/^fpr:/ {print \$10; exit}"')
key=${fpr: -16}
if [ -z "$fpr" ]; then
    note "FAIL: テスト鍵を作れない"
    cat /tmp/gpg.log
    exit 1
fi
su tester -c "gpg --batch --pinentry-mode loopback --passphrase test-password \
    --armor --export-secret-keys '$fpr'" >/etc/nekonolock/sudo-key.asc
cat >/etc/nekonolock/sudo.toml <<TOML
[sudo]
enabled = true
key = "$key"
public_key = "/etc/nekonolock/sudo-key.asc"
challenge_bytes = 32
timeout_sec = 15
users = []
TOML
chmod 644 /etc/nekonolock/sudo.toml /etc/nekonolock/sudo-key.asc
chmod 755 /etc/nekonolock

# 実カードの代わりに、信頼した鍵ファイルへソフトウェア秘密鍵を置く。
# PAM モジュールは root 専用の homedir にこれを取り込み、root の agent で
# 署名する。ユーザーの gpg-agent は使わない。
su tester -c 'gpgconf --kill gpg-agent >/dev/null 2>&1 || true'
if su tester -c 'printf "%s\n" test-password | sudo -S -k /bin/true' >/tmp/f2.log 2>&1; then
    note "ok: 正しい PIN で sudo が成功"
else
    note "FAIL: 正しい PIN で sudo が失敗した"
    cat /tmp/f2.log
    fail=1
fi

# 直前の成功でユーザーの gpg-agent は PIN をキャッシュしているが、sudo は
# root 専用の一時 agent（ignore-cache-for-signing、TTL 0）で署名するため、
# ユーザーの agent を止めなくても間違った PIN は拒否される。
if su tester -c 'printf "%s\n" wrong-pin | sudo -S -k /bin/true' >/tmp/f3.log 2>&1; then
    note "FAIL: 間違った PIN で sudo が成功した"
    cat /tmp/f3.log
    fail=1
else
    note "ok: 間違った PIN は pam_deny まで進む"
fi

if [ "$fail" -ne 0 ]; then
    note "FAIL"
    exit 1
fi
note "PASS"
EOF

docker run --rm ${DOCKER_ARGS:-} \
    -v "$root:/work" \
    -v "$work:/out" \
    "$image" bash /out/guest.sh
