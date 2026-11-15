#!/bin/bash
# Phase 4: packaging (PKGBUILD) と ansible role を Arch Linux コンテナで検証する。
#
# 確認すること:
#   1. makepkg で package が作れる（check() の cargo test を含む）
#   2. pacman -U で入り、バイナリ・PAM・systemd user unit・LICENSE が配置される
#   3. unit が SOCK_SEQPACKET を使い、systemd-analyze verify を通る
#   4. ansible role が PAM を置き、swayidle の行を nekonolock に差し替える
#   5. role を 2 回適用しても changed が出ない
#
# 前提:
#   - Docker（rootless 可）
#   - 初回は Arch ミラー / GitHub / crates.io へ接続できるネットワーク
#
# 使い方:
#   bash spike/04-package-ansible.sh
#
# 環境変数:
#   CA_CERT         追加で信頼する CA 証明書（TLS を検査する環境向け。任意）
#   DOCKER_ARGS     docker run に渡す追加引数（例: --net=host）
#   NEKONOLOCK_SRC  事前に取得した source tarball（省略時は makepkg が取得）
#   KEEP=1          終了時に WORK を残す
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=${WORK:-$(mktemp -d)}
if [ -z "${KEEP:-}" ]; then
    trap 'rm -rf "$work"' EXIT
fi
mkdir -p "$work"
echo "==> work: $work"

cp -a "$root/packaging" "$work/packaging"
cp "$root/LICENSE" "$work/LICENSE"

# PKGBUILD の source から upstream tarball の名前と URL を取る。
src_name=$(bash -c 'set -e; cd "$1"; source PKGBUILD; echo "${source[0]%%::*}"' _ "$work/packaging")
src_url=$(bash -c 'set -e; cd "$1"; source PKGBUILD; echo "${source[0]#*::}"' _ "$work/packaging")
if [ -n "${NEKONOLOCK_SRC:-}" ]; then
    cp "$NEKONOLOCK_SRC" "$work/packaging/$src_name"
else
    curl_args=()
    if [ -n "${CA_CERT:-}" ]; then
        curl_args=(--cacert "$CA_CERT")
    fi
    curl "${curl_args[@]}" -fsSL -o "$work/packaging/$src_name" "$src_url"
fi

if [ -n "${CA_CERT:-}" ]; then
    cp "$CA_CERT" "$work/ca.crt"
else
    : >"$work/ca.crt"
fi

cat >"$work/Dockerfile" <<'EOF'
FROM archlinux:latest
COPY ca.crt /tmp/ca.crt
RUN if [ -s /tmp/ca.crt ]; then cat /tmp/ca.crt >> /etc/ssl/certs/ca-certificates.crt; fi \
 && pacman -Syu --noconfirm \
 && pacman -S --noconfirm --needed \
      base-devel cargo libxkbcommon wayland gnupg pam ttf-dejavu ansible \
 && useradd -m builder
EOF
docker build -q -t local/nekonolock-pkg:latest -f "$work/Dockerfile" "$work"

cat >"$work/guest.sh" <<'EOF'
#!/bin/bash
set -uo pipefail
fail=0
note() { echo "phase4: $*"; }
check() { if eval "$2"; then note "ok: $1"; else note "FAIL: $1"; fail=1; fi; }

# 1. 一般ユーザーで package を作る（makepkg は root では動かない）。
# rootless Docker では /work が container root の 0700 なので、builder が
# 辿れるようにする。最後に root へ戻して host 側で削除できるようにする。
chmod 755 /work
build_cleanup() { chown -R root:root /work/build 2>/dev/null || true; }
trap build_cleanup EXIT
install -d -o builder -g builder /work/build
cp -a /work/packaging /work/build/
chown -R builder:builder /work/build
if su builder -c 'cd /work/build/packaging && SRCDEST=/work/build/packaging makepkg -f --nodeps' \
    >/tmp/makepkg.log 2>&1; then
    note "ok: makepkg"
else
    note "FAIL: makepkg"
    tail -80 /tmp/makepkg.log
    exit 1
fi

# makepkg は debug package も作る。main package だけを選ぶ。
pkg=$(find /work/build/packaging -maxdepth 1 \
    -name 'nekonolock-[0-9]*.pkg.tar.zst' | head -1)
check "package ができた" "[ -n '$pkg' ]"

su builder -c 'cd /work/build/packaging && makepkg --printsrcinfo' >/tmp/srcinfo 2>/tmp/srcinfo.err
if diff -u /work/build/packaging/.SRCINFO /tmp/srcinfo >/tmp/srcinfo.diff 2>&1; then
    note "ok: .SRCINFO は PKGBUILD と同期"
else
    note "FAIL: .SRCINFO が PKGBUILD と一致しない"
    tail -40 /tmp/srcinfo.diff
    fail=1
fi
if pacman -U --noconfirm "$pkg" >/tmp/pacman.log 2>&1; then
    note "ok: pacman -U"
else
    note "FAIL: pacman -U"
    tail -40 /tmp/pacman.log
    fail=1
fi

check "nekonolock がある" "[ -x /usr/bin/nekonolock ]"
check "nekonolock-auth がある" "[ -x /usr/bin/nekonolock-auth ]"
check "PAM の auth" "grep -qx 'auth     include login' /etc/pam.d/nekonolock"
check "PAM の account" "grep -qx 'account  include login' /etc/pam.d/nekonolock"
check "socket は SOCK_SEQPACKET" \
    "grep -qx 'ListenSequentialPacket=%t/nekonolock/auth.sock' /usr/lib/systemd/user/nekonolock-auth.socket"
check "service は socket を要求" \
    "grep -qx 'Requires=nekonolock-auth.socket' /usr/lib/systemd/user/nekonolock-auth.service"
check "sudo 用設定の例がある" "[ -f /usr/share/doc/nekonolock/sudo.toml.example ]"
check "LICENSE がある" "[ -f /usr/share/licenses/nekonolock/LICENSE ]"
check "LICENSE は repo と同一" "cmp -s /usr/share/licenses/nekonolock/LICENSE /work/LICENSE"
check "nekonolock --help" "/usr/bin/nekonolock --help >/dev/null 2>&1"
check "nekonolock-auth --help" "/usr/bin/nekonolock-auth --help >/dev/null 2>&1"

systemd-analyze verify /usr/lib/systemd/user/nekonolock-auth.socket \
    /usr/lib/systemd/user/nekonolock-auth.service >/tmp/verify.log 2>&1
verify_rc=$?
if [ "$verify_rc" -ne 0 ]; then
    sed 's/^/  /' /tmp/verify.log
fi
check "systemd-analyze verify" "[ $verify_rc -eq 0 ]"

# 2. ansible role。package はすでに入っているので、PAM と swayidle を確認する。
cat >/root/sway-config <<'SWAY'
set $mod Mod4
exec swayidle -w timeout 300 'swaylock -f' timeout 600 'swaymsg "output * power off"' resume 'swaymsg "output * power on"' before-sleep 'swaylock -f'
bindsym $mod+Escape exec swaylock
SWAY
cat >/work/playbook.yml <<'PLAY'
- hosts: localhost
  connection: local
  gather_facts: true
  vars:
    nekonolock_user: root
    nekonolock_enable_user_socket: false
    nekonolock_sway_config: /root/sway-config
  roles:
    - nekonolock
PLAY

export ANSIBLE_ROLES_PATH=/work/build/packaging/ansible/roles
if ansible-playbook -i localhost, -c local /work/playbook.yml >/tmp/ansible.log 2>&1; then
    note "ok: ansible-playbook"
else
    note "FAIL: ansible-playbook"
    tail -60 /tmp/ansible.log
    fail=1
fi
check "swayidle が nekonolock になった" \
    "grep -q 'timeout 300 .nekonolock.' /root/sway-config"
check "swayidle の before-sleep も nekonolock" \
    "grep -q 'before-sleep .nekonolock.' /root/sway-config"
check "swayidle 以外の swaylock は残る" \
    "grep -q 'bindsym .*exec swaylock' /root/sway-config"
check "PAM は ansible が管理" "grep -q 'Managed by ansible' /etc/pam.d/nekonolock"

if ansible-playbook -i localhost, -c local /work/playbook.yml >/tmp/ansible2.log 2>&1; then
    note "ok: ansible-playbook (2 回目)"
else
    note "FAIL: ansible-playbook (2 回目)"
    tail -60 /tmp/ansible2.log
    fail=1
fi
check "role は冪等" "grep -q 'changed=0' /tmp/ansible2.log"

if [ "$fail" -ne 0 ]; then
    note "FAIL"
    exit 1
fi
note "PASS"
EOF

docker run --rm ${DOCKER_ARGS:-} \
    -v "$work:/work" \
    local/nekonolock-pkg:latest bash /work/guest.sh
