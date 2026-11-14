#!/bin/bash
# Phase 1 spike: ext-session-lock-v1 で全出力をロックできるかを VM で確認する。
#
# やること:
#   1. `crates/lock` のリリースバイナリを `--features insecure-test-unlock`
#      付きでビルドする（既定ビルドはキー入力を無視するため）。
#   2. Arch Linux のゲスト rootfs（Sway + wtype）を Docker で作る。
#   3. QEMU に Arch のカーネルと initramfs を渡して起動する。
#   4. ゲスト内で Sway を headless バックエンドで起動し、nekonolock が
#      2 出力をロックし、出力追加に追従し、キー入力で解除することを確認する。
#
# 前提:
#   - Docker（rootless 可）と /dev/kvm
#   - cargo（NEKONOLOCK_BIN を指定した場合は不要）
#   - Arch ミラーへ接続できるネットワーク
#
# 使い方:
#   bash spike/02-ext-session-lock-vm.sh
#
# 環境変数:
#   NEKONOLOCK_BIN  ビルド済みバイナリ（指定時は cargo build を省略。
#                   必ず --features insecure-test-unlock 付きでビルドする）
#   WORK            作業ディレクトリ（既定: mktemp -d）
#   KEEP=1          終了時に WORK を残す
#   EXTRA_CA_CERT   TLS を傍受する環境で Arch ミラーに接続するための CA PEM
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=${WORK:-$(mktemp -d)}
if [ -z "${KEEP:-}" ]; then
    trap 'rm -rf "$work"' EXIT
fi

mkdir -p "$work/docker" "$work/overlay/usr/local/bin" \
    "$work/overlay/etc/systemd/system/multi-user.target.wants"

echo "==> work: $work"

# 1. バイナリ
if [ -n "${NEKONOLOCK_BIN:-}" ]; then
    cp "$NEKONOLOCK_BIN" "$work/overlay/usr/local/bin/nekonolock"
else
    cargo build --release --manifest-path "$root/Cargo.toml" -p nekonolock \
        --features insecure-test-unlock
    cp "$root/target/release/nekonolock" "$work/overlay/usr/local/bin/nekonolock"
fi
chmod 755 "$work/overlay/usr/local/bin/nekonolock"

# 2. ベースイメージ（CA を足す場合だけ派生させる）
base_image=archlinux:latest
if [ -n "${EXTRA_CA_CERT:-}" ]; then
    cp "$EXTRA_CA_CERT" "$work/docker/extra-ca.pem"
    cat >"$work/docker/Dockerfile.base" <<'EOF'
FROM archlinux:latest
COPY extra-ca.pem /usr/local/share/ca-certificates/extra.crt
RUN cat /usr/local/share/ca-certificates/extra.crt >> /etc/ssl/certs/ca-certificates.crt
EOF
    docker build -q -t nekonolock-spike-base \
        -f "$work/docker/Dockerfile.base" "$work/docker" >/dev/null
    base_image=nekonolock-spike-base
fi

# 3. ゲスト rootfs（Sway と検証用ツール）
cat >"$work/docker/Dockerfile.guest" <<'EOF'
ARG BASE_IMAGE=archlinux:latest
FROM ${BASE_IMAGE}
RUN pacman -Syu --noconfirm
RUN pacman -S --noconfirm --needed sway wtype grim dbus ttf-dejavu linux mkinitcpio
RUN sed -i 's/^MODULES=.*/MODULES=(virtio_pci virtio_blk virtio_net virtio_console ext4)/' /etc/mkinitcpio.conf \
 && sed -i 's/^HOOKS=.*/HOOKS=(base udev modconf keyboard keymap consolefont block filesystems fsck)/' /etc/mkinitcpio.conf \
 && mkinitcpio -P
EOF
docker build -q --build-arg BASE_IMAGE="$base_image" -t nekonolock-spike-guest \
    -f "$work/docker/Dockerfile.guest" "$work/docker" >/dev/null

# 4. QEMU
cat >"$work/docker/Dockerfile.qemu" <<'EOF'
ARG BASE_IMAGE=archlinux:latest
FROM ${BASE_IMAGE}
RUN pacman -Syu --noconfirm
RUN pacman -S --noconfirm --needed qemu-system-x86 qemu-img
ENTRYPOINT ["qemu-system-x86_64"]
EOF
docker build -q --build-arg BASE_IMAGE="$base_image" -t nekonolock-spike-qemu \
    -f "$work/docker/Dockerfile.qemu" "$work/docker" >/dev/null

# 5. ゲスト内の検証スクリプト
cat >"$work/overlay/usr/local/bin/nekonolock-vm-test.sh" <<'EOF'
#!/bin/bash
set -u

fail=0
note() { echo "nekonolock-test: $*"; }

export XDG_RUNTIME_DIR=/run/user/0
install -d -m 700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless
export WLR_HEADLESS_OUTPUTS=2
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER=pixman

note "start"
note "kernel: $(uname -r)"
note "sway: $(sway --version | head -1)"

sway --debug >/tmp/sway.log 2>&1 &
sway_pid=$!

sock=""
for _ in $(seq 1 100); do
    sock=$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -name 'wayland-*' -type s 2>/dev/null | head -1)
    [ -n "$sock" ] && break
    sleep 0.1
done
if [ -z "$sock" ]; then
    note "FAIL sway: socket が出ない"
    tail -60 /tmp/sway.log
    poweroff -f
fi
export WAYLAND_DISPLAY=$(basename "$sock")
notify_sock=$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -name 'sway-ipc.*.sock' 2>/dev/null | head -1)
[ -n "$notify_sock" ] && export SWAYSOCK="$notify_sock"
note "WAYLAND_DISPLAY=$WAYLAND_DISPLAY SWAYSOCK=${SWAYSOCK:-}"

/usr/local/bin/nekonolock >/tmp/lock.log 2>&1 &
lock_pid=$!

locked=0
for _ in $(seq 1 100); do
    if grep -q '^locked$' /tmp/lock.log 2>/dev/null \
        && [ "$(grep -c '^lock-surface: created ' /tmp/lock.log 2>/dev/null)" -ge 2 ]; then
        locked=1
        break
    fi
    kill -0 "$lock_pid" 2>/dev/null || break
    sleep 0.1
done
note "lock log:"
sed 's/^/  /' /tmp/lock.log
if [ "$locked" -ne 1 ]; then
    note "FAIL lock: 2 出力のロックサーフェスができていない"
    fail=1
fi

# 出力の動的追加に追従するか。
swaymsg create_output >/tmp/create-output.log 2>&1 || note "create_output: $(cat /tmp/create-output.log)"
sleep 1
outputs=$(grep '^lock-surface: created ' /tmp/lock.log | awk '{print $3}' | sort -u | tr '\n' ' ')
note "outputs: $outputs"
if [ "$(printf '%s\n' $outputs | wc -l)" -ge 3 ]; then
    note "output-add: OK"
else
    note "output-add: FAIL"
    fail=1
fi

# キー入力で解除されるか。Sway の virtual-keyboard を使う。
for _ in $(seq 1 20); do
    wtype -k a >/tmp/wtype.log 2>&1 && break
    sleep 0.2
done
for _ in $(seq 1 50); do
    kill -0 "$lock_pid" 2>/dev/null || break
    sleep 0.1
done
if kill -0 "$lock_pid" 2>/dev/null; then
    note "FAIL key: キー入力後も終了しない"
    kill "$lock_pid" 2>/dev/null
    fail=1
else
    wait "$lock_pid"
    rc=$?
    note "lock exit=$rc"
    [ "$rc" -eq 0 ] || fail=1
fi

for word in key-pressed unlocked done; do
    if grep -qx "$word" /tmp/lock.log; then
        note "$word: OK"
    else
        note "FAIL: $word が無い"
        fail=1
    fi
done

kill "$sway_pid" 2>/dev/null
wait "$sway_pid" 2>/dev/null
note "result: $fail"
poweroff -f
EOF
chmod 755 "$work/overlay/usr/local/bin/nekonolock-vm-test.sh"

cat >"$work/overlay/etc/systemd/system/nekonolock-vm-test.service" <<'EOF'
[Unit]
Description=nekonolock Phase 1 VM smoke test
After=systemd-user-sessions.service

[Service]
Type=oneshot
ExecStart=/usr/local/bin/nekonolock-vm-test.sh
StandardOutput=journal+console
StandardError=journal+console
TimeoutStartSec=180

[Install]
WantedBy=multi-user.target
EOF
ln -s /etc/systemd/system/nekonolock-vm-test.service \
    "$work/overlay/etc/systemd/system/multi-user.target.wants/nekonolock-vm-test.service"
printf '0123456789abcdef0123456789abcdef\n' >"$work/overlay/etc/machine-id"
ln -s /dev/null "$work/overlay/etc/systemd/system/systemd-firstboot.service"

# 6. rootfs イメージを作る（root 権限が要らないようコンテナ内で mkfs）
container="nekonolock-spike-$$"
docker create --name "$container" nekonolock-spike-guest >/dev/null
docker export "$container" -o "$work/rootfs.tar"
docker rm "$container" >/dev/null

docker run --rm -v "$work:/vm" nekonolock-spike-guest bash -c '
    set -e
    mkdir -p /rootfs
    tar -xpf /vm/rootfs.tar -C /rootfs
    cp -a /vm/overlay/. /rootfs/
    cp /rootfs/boot/vmlinuz-linux /vm/vmlinuz-linux
    cp /rootfs/boot/initramfs-linux.img /vm/initramfs-linux.img
    truncate -s 6G /vm/rootfs.img
    mkfs.ext4 -q -F -L nekonolock -d /rootfs /vm/rootfs.img
'

# 7. 起動して結果を確認する
if [ ! -e /dev/kvm ]; then
    echo "FAIL: /dev/kvm が無い。KVM を有効にした環境で実行する。" >&2
    exit 1
fi

kernel_args='root=/dev/vda rw rootfstype=ext4 console=ttyS0,115200 loglevel=4 systemd.unit=multi-user.target systemd.log_color=0'
set +e
timeout 300 docker run --rm --device /dev/kvm -v "$work:/vm" nekonolock-spike-qemu \
    -enable-kvm -cpu host -smp 2 -m 3072 \
    -kernel /vm/vmlinuz-linux -initrd /vm/initramfs-linux.img \
    -append "$kernel_args" \
    -drive file=/vm/rootfs.img,format=raw,if=virtio,cache=unsafe \
    -nographic -no-reboot >"$work/qemu.log" 2>&1
qemu_rc=$?
set -e

sed -n 's/^.*nekonolock-test: /  /p' "$work/qemu.log" | tr -d '\r'
if [ "$qemu_rc" -ne 0 ]; then
    echo "FAIL: qemu が終了コード $qemu_rc で止まった" >&2
    tail -40 "$work/qemu.log" >&2
    exit 1
fi
if ! grep -aq 'nekonolock-test: result: 0' "$work/qemu.log"; then
    echo "FAIL: ゲスト内の検証が失敗した" >&2
    tail -40 "$work/qemu.log" >&2
    exit 1
fi

echo "PASS: ext-session-lock-v1 の VM 検証が通った"
