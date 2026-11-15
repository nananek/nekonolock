#!/bin/bash
# Phase 3: ロック画面（crates/lock）を Sway の headless 出力で検証する。
#
# 確認すること:
#   1. 2 出力にロックサーフェスができ、出力の追加・削除に追従する
#   2. 誤 PIN ではロックが解けない
#   3. Tab でパスワードへ切り替え、正しいパスワードで解除できる
#   4. カード無し + パスワード無効では、PIN 要求を no-card で拒否してロックを維持する
#   5. 認証ヘルパが無くてもロックを維持する
#
# 前提:
#   - Docker（rootless 可）
#   - 初回は Arch ミラーへ接続できるネットワーク（local/arch-sway を作るため）
#   - バイナリを指定しない場合はホストの cargo
#
# 使い方:
#   bash spike/03-lock-ui-sway.sh
#
# 環境変数:
#   NEKONOLOCK_BIN  ビルド済み nekonolock（省略時は cargo build）
#   AUTH_BIN        ビルド済み nekonolock-auth（mock-card,mock-pam 必須）
#   WKEY_BIN        ビルド済み wkey（省略時は spike/wkey をビルド）
#   KEEP=1          終了時に WORK を残す
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=${WORK:-$(mktemp -d)}
if [ -z "${KEEP:-}" ]; then
    trap 'rm -rf "$work"' EXIT
fi

echo "==> work: $work"

# 1. バイナリ
if [ -n "${NEKONOLOCK_BIN:-}" ]; then
    cp "$NEKONOLOCK_BIN" "$work/nekonolock"
else
    cargo build --manifest-path "$root/Cargo.toml" -p nekonolock
    cp "$root/target/debug/nekonolock" "$work/nekonolock"
fi

if [ -n "${AUTH_BIN:-}" ]; then
    cp "$AUTH_BIN" "$work/nekonolock-auth"
else
    cargo build --manifest-path "$root/Cargo.toml" -p nekonolock-auth \
        --features mock-card,mock-pam
    cp "$root/target/debug/nekonolock-auth" "$work/nekonolock-auth"
fi

wkey=${WKEY_BIN:-}
if [ -z "$wkey" ] || [ ! -x "$wkey" ]; then
    CARGO_TARGET_DIR="$work/wkey-target" cargo build --manifest-path "$root/spike/wkey/Cargo.toml"
    wkey="$work/wkey-target/debug/wkey"
fi
cp "$wkey" "$work/wkey"
chmod 755 "$work/nekonolock" "$work/nekonolock-auth" "$work/wkey"

# 2. Sway / xkbcli / DejaVu の入った検証用イメージ
if ! docker image inspect local/arch-sway:latest >/dev/null 2>&1; then
    cat >"$work/Dockerfile.arch-sway" <<'EOF'
FROM archlinux:latest
RUN pacman -Syu --noconfirm \
 && pacman -S --noconfirm --needed sway libxkbcommon ttf-dejavu
EOF
    docker build -q -t local/arch-sway:latest -f "$work/Dockerfile.arch-sway" "$work"
fi

# 3. 設定
cat >"$work/config-both.toml" <<'EOF'
[ui]
clock_format = "%H:%M"
font = "/usr/share/fonts/TTF/DejaVuSans.ttf"
clock_font = "/usr/share/fonts/TTF/DejaVuSans-ExtraLight.ttf"
max_attempts = 5

[auth]
openpgp = true
password = true
key = "0123456789ABCDEF!"
challenge_bytes = 32
timeout_sec = 10

[pam]
service = "nekonolock-test"
EOF

cat >"$work/config-pin-only.toml" <<'EOF'
[ui]
font = "/usr/share/fonts/TTF/DejaVuSans.ttf"
clock_font = "/usr/share/fonts/TTF/DejaVuSans-ExtraLight.ttf"
max_attempts = 5

[auth]
openpgp = true
password = false
key = "0123456789ABCDEF!"
challenge_bytes = 32
timeout_sec = 10

[pam]
service = "nekonolock-test"
EOF

# 4. ゲスト内の検証
cat >"$work/guest.sh" <<'EOF'
#!/bin/bash
set -u
fail=0
note() { echo "phase3: $*"; }
check() { if eval "$2"; then note "ok: $1"; else note "FAIL: $1"; fail=1; fi; }

export XDG_RUNTIME_DIR=/run/user/0
install -d -m 700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless
export WLR_HEADLESS_OUTPUTS=2
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman

sway --debug >/tmp/sway.log 2>&1 &
sway_pid=$!
sock=""
for _ in $(seq 1 100); do
    sock=$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -name 'wayland-*' -type s 2>/dev/null | head -1)
    [ -n "$sock" ] && break
    sleep 0.1
done
if [ -z "$sock" ]; then
    note "FAIL: sway の socket が出ない"
    tail -40 /tmp/sway.log
    exit 1
fi
export WAYLAND_DISPLAY=$(basename "$sock")
notify=$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -name 'sway-ipc.*.sock' 2>/dev/null | head -1)
[ -n "$notify" ] && export SWAYSOCK="$notify"
xkbcli compile-keymap --layout us >/tmp/us.xkb

auth_sock="$XDG_RUNTIME_DIR/nekonolock/auth.sock"
start_helper() {
    local config=$1
    shift
    install -d -m 700 "$XDG_RUNTIME_DIR/nekonolock"
    rm -f "$auth_sock"
    env "$@" /usr/local/bin/nekonolock-auth --config "$config" --socket "$auth_sock" \
        >/tmp/auth.log 2>&1 &
    auth_pid=$!
    for _ in $(seq 1 100); do [ -S "$auth_sock" ] && return 0; sleep 0.1; done
    return 1
}
stop_helper() {
    if [ -n "${auth_pid:-}" ]; then
        kill "$auth_pid" 2>/dev/null || true
        wait "$auth_pid" 2>/dev/null || true
        auth_pid=""
    fi
    rm -f "$auth_sock"
}
start_lock() {
    /usr/local/bin/nekonolock --config "$1" >/tmp/lock.log 2>&1 &
    lock_pid=$!
    for _ in $(seq 1 100); do
        grep -q '^locked$' /tmp/lock.log 2>/dev/null && return 0
        kill -0 "$lock_pid" 2>/dev/null || return 1
        sleep 0.1
    done
    return 1
}
wait_surfaces() {
    local want=$1
    for _ in $(seq 1 100); do
        local got
        got=$(grep -c '^lock-surface: created ' /tmp/lock.log 2>/dev/null || true)
        [ "$got" -ge "$want" ] && return 0
        sleep 0.1
    done
    return 1
}
wait_exit() {
    for _ in $(seq 1 100); do
        if ! kill -0 "$lock_pid" 2>/dev/null; then
            wait "$lock_pid"
            return $?
        fi
        sleep 0.1
    done
    return 124
}
stop_lock() {
    if [ -n "${lock_pid:-}" ]; then
        kill "$lock_pid" 2>/dev/null || true
        wait "$lock_pid" 2>/dev/null || true
        lock_pid=""
    fi
}
stop_all() {
    stop_lock
    stop_helper
    kill "$sway_pid" 2>/dev/null || true
    wait "$sway_pid" 2>/dev/null || true
}
trap stop_all EXIT

# --- 0. 正しい PIN で解除 ---
note "scenario 0: correct PIN unlock"
start_helper /work/config-both.toml
start_lock /work/config-both.toml || { note "FAIL: lock が locked にならない"; sed 's/^/  /' /tmp/lock.log; exit 1; }
check "2 出力に lock surface（PIN）" "wait_surfaces 2"
/usr/local/bin/wkey /tmp/us.xkb 2 3 4 5 6 7 28 >/tmp/wkey.log 2>&1
if wait_exit; then
    note "ok: 正しい PIN で解除 (rc=0)"
    check "openpgp 方式で解除" "grep -q 'unlocked method=openpgp' /tmp/auth.log"
else
    note "FAIL: 正しい PIN で解除できない"; fail=1
    sed 's/^/  /' /tmp/lock.log
fi
stop_lock
stop_helper

# --- 1. PIN の失敗、出力の追加・削除、Tab でパスワード ---
note "scenario 1: pin failure, output changes, password unlock"
start_helper /work/config-both.toml
start_lock /work/config-both.toml || { note "FAIL: lock が locked にならない"; sed 's/^/  /' /tmp/lock.log; exit 1; }
check "2 出力に lock surface" "wait_surfaces 2"
check "2 出力に lock surface" "[ \"$(grep -c '^lock-surface: created ' /tmp/lock.log)\" -ge 2 ]"

# 000000 + Enter
/usr/local/bin/wkey /tmp/us.xkb 11 11 11 11 11 11 28 >/tmp/wkey.log 2>&1
sleep 0.5
check "誤 PIN を拒否" "grep -q 'reason=auth-failed' /tmp/auth.log"
check "誤 PIN 後もロック維持" "kill -0 $lock_pid 2>/dev/null"

swaymsg create_output >/tmp/create.log 2>&1 || note "create_output: $(cat /tmp/create.log)"
sleep 1
check "出力追加に追従" "[ \"$(grep -c '^lock-surface: created ' /tmp/lock.log)\" -ge 3 ]"
if swaymsg output HEADLESS-3 unplug >/tmp/unplug.log 2>&1; then
    sleep 1
    check "出力削除に追従" "grep -q 'lock-surface: removed HEADLESS-3' /tmp/lock.log"
else
    note "SKIP: 出力削除（$(cat /tmp/unplug.log)）"
fi

# Tab + test-password + Enter（1 回の wkey で送る。仮想キーボードの
# 作成・削除とキー送出の競合を避ける）
/usr/local/bin/wkey /tmp/us.xkb 15 20 18 31 20 12 25 30 31 31 17 24 19 32 28 >/tmp/wkey.log 2>&1
if wait_exit; then
    note "ok: 正しいパスワードで解除 (rc=0)"
    check "password 方式で解除" "grep -q 'unlocked method=password' /tmp/auth.log"
else
    note "FAIL: 正しいパスワードで解除できない"; fail=1
    sed 's/^/  /' /tmp/lock.log
fi
stop_lock
stop_helper

# --- 2. カード無し + パスワード無効: no-card でロック維持 ---
note "scenario 2: no card, password disabled"
start_helper /work/config-pin-only.toml NEKONOLOCK_MOCK_CARD=absent
start_lock /work/config-pin-only.toml || { note "FAIL: lock が locked にならない"; fail=1; }
check "lock surface（no card）" "wait_surfaces 1"
/usr/local/bin/wkey /tmp/us.xkb 2 3 4 5 6 7 28 >/tmp/wkey.log 2>&1
sleep 0.5
check "no-card を返す" "grep -q 'reason=no-card' /tmp/auth.log"
check "no-card 後もロック維持" "kill -0 $lock_pid 2>/dev/null"
stop_lock
stop_helper

# --- 3. ヘルパ無し: 接続エラーでもロック維持 ---
note "scenario 3: auth helper unavailable"
start_lock /work/config-pin-only.toml || { note "FAIL: lock が locked にならない"; fail=1; }
check "lock surface（helper 無し）" "wait_surfaces 1"
/usr/local/bin/wkey /tmp/us.xkb 2 3 4 5 6 7 28 >/tmp/wkey.log 2>&1
sleep 0.5
check "ヘルパ不達を表示" "grep -q '認証ヘルパを利用できない' /tmp/lock.log"
check "ヘルパ不達でもロック維持" "kill -0 $lock_pid 2>/dev/null"
stop_lock

if [ "$fail" -ne 0 ]; then
    note "FAIL"
    exit 1
fi
note "PASS"
exit 0
EOF

docker run --rm --cap-add=SYS_NICE \
    -v "$work:/work:ro" \
    -v "$work/nekonolock:/usr/local/bin/nekonolock:ro" \
    -v "$work/nekonolock-auth:/usr/local/bin/nekonolock-auth:ro" \
    -v "$work/wkey:/usr/local/bin/wkey:ro" \
    local/arch-sway:latest bash /work/guest.sh
