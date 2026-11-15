#!/bin/bash
# Phase 7: sudo 用 PAM モジュールを Docker 内でビルドし、PAM スタックから
# pam_authenticate を呼ぶ。
#
# 実カードは使わない。テストユーザーのソフトウェア GPG 鍵で署名し、
# `gpg --card-status` だけを偽装して、署名・検証・PIN の経路を通す。
# 確認すること:
#   1. 設定が無いときは PAM_IGNORE を返し、パスワード側へフォールバックする
#   2. 正しい PIN（鍵のパスフレーズ）なら pam_authenticate が成功する
#   3. 間違った PIN なら PAM_IGNORE を返し、required の pam_deny で失敗する
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

# 3. ソフトウェア鍵を作り、card-status だけを偽装する。
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
su tester -c "gpg --batch --armor --export '$fpr'" >/etc/nekonolock/sudo-key.asc
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

mv /usr/bin/gpg /usr/bin/gpg.real
cat >/tmp/fakegpg.c <<'C'
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--card-status") == 0) {
            printf("General key info..: pub  ed25519/%s 2026-11-15 Tester <tester@example>\n",
                   KEY_ID);
            printf("Signature key ....: %s\n", FPR);
            return 0;
        }
    }
    execv("/usr/bin/gpg.real", argv);
    perror("execv");
    return 127;
}
C
gcc -O2 -DKEY_ID="\"$key\"" -DFPR="\"$fpr\"" -o /usr/bin/gpg /tmp/fakegpg.c
chmod 755 /usr/bin/gpg

su tester -c 'gpgconf --kill gpg-agent >/dev/null 2>&1 || true'
if su tester -c 'printf "%s\n" test-password | sudo -S -k /bin/true' >/tmp/f2.log 2>&1; then
    note "ok: 正しい PIN で sudo が成功"
else
    note "FAIL: 正しい PIN で sudo が失敗した"
    cat /tmp/f2.log
    fail=1
fi

# 直前の成功で gpg-agent が PIN をキャッシュしている。キャッシュが残ると
# 間違った PIN でも署名できてしまうため、テストの前提として消す。
su tester -c 'gpgconf --kill gpg-agent >/dev/null 2>&1 || true'
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
