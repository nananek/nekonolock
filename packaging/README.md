# packaging

pacman package `nekonolock` と ansible role `nekonolock` のステージング用。
このディレクトリの内容は、それぞれ次の repository へ移植する。

| ここ | 移植先 |
|---|---|
| `PKGBUILD`, `nekonolock.pam`, `nekonolock-auth.{socket,service}`, `LICENSE` | `nekono-pacman-distribution/pkgs/nekonolock/` |
| `ansible/roles/nekonolock/` | `ansible-nekonodesk/roles/nekonolock/` |

## package の中身

| path | 内容 |
|---|---|
| `/usr/bin/nekonolock` | ロック画面（`ext-session-lock-v1`） |
| `/usr/bin/nekonolock-auth` | 認証ヘルパ（OpenPGP / PAM） |
| `/etc/pam.d/nekonolock` | パスワード認証。`auth` と `account` の両方で `login` を include する |
| `/usr/lib/systemd/user/nekonolock-auth.socket` | `SOCK_SEQPACKET` の socket activation |
| `/usr/lib/systemd/user/nekonolock-auth.service` | 認証ヘルパ本体 |
| `/usr/share/licenses/nekonolock/LICENSE` | MIT |

`ListenSequentialPacket=` を使う。`ListenStream=` にすると `SOCK_STREAM` が
でき、ヘルパが `WrongSocketType` で起動を拒否する（設計 §6）。

`socket` だけを enable する。`service` は socket activation 専用なので
`[Install]` を持たない。

## package のビルド

`makepkg` は root では実行できない。Arch Linux 上で一般ユーザーとして実行する。

```sh
cd packaging
makepkg --syncdeps --cleanbuild --force
makepkg --printsrcinfo > .SRCINFO
```

依存は `gnupg` / `libxkbcommon` / `pam` / `ttf-dejavu`、ビルドに `rust`
（`cargo` を含む）。YubiKey を pcscd 経由で使うホストでは `pcsclite` と
`ccid` も入れる（`optdepends`）。

`source` は v0.2.1 から GitHub release tag の archive を pin している。
`[nekono]` repo の `pkgs/nekonolock/` も同じ tag archive と sha256 を使う。
新しい tag を切ったら、`pkgver` を上げて次のように `sha256sums` を再計算する。

```sh
curl -fsSL -o /tmp/nekonolock-0.3.0.tar.gz \
  https://github.com/nananek/nekonolock/archive/refs/tags/v0.3.0.tar.gz
sha256sum /tmp/nekonolock-0.3.0.tar.gz
```

`source` の先頭は `$pkgname-$pkgver.tar.gz::.../archive/refs/tags/v$pkgver.tar.gz`
のままでよい。tag archive の sha256 だけを差し替える。

## 検証

`spike/04-package-ansible.sh` が Arch Linux コンテナ内で次を実行する。

1. `makepkg` で package を作る
2. `pacman -U` で入れ、2 本のバイナリと同梱ファイルを確認する
3. `ansible-playbook` で role を `connection: local` に適用する
4. PAM ファイルと swayidle の差し替え結果を確認する

```sh
bash spike/04-package-ansible.sh
```

## [nekono] repo への移植

1. `pkgs/nekonolock/` に `PKGBUILD` とローカル source
   （`nekonolock.pam`, `nekonolock-auth.socket`, `nekonolock-auth.service`,
   `LICENSE`）を置く。
2. `pkgver` を literal のまま、`arch=('x86_64')` を維持する。
3. `.SRCINFO` を生成し、`.deps.lock` を repo の手順で初期化する。
4. `nvchecker.toml` に section を追加する（tag を切ってから）。
5. `REVIEW.md` に source URL、sha256 の実測値、依存の根拠、socket unit が
   `SOCK_SEQPACKET` であることを書く。

## ansible role の移植

`ansible/roles/nekonolock/` を `ansible-nekonodesk/roles/` にコピーする。
前提は次のとおり。

- `[nekono]` repo が登録済みで、`nekonolock` package を install できる。
- ansible は `community.general` の `pacman` モジュールを使う
  （Arch の `ansible` package には含まれる。`ansible-core` だけでは足りない）。
- role は対象ユーザーのログイン中に実行する。ユーザー manager が無い場合は
  socket を有効化せず警告する。
- `~/.config/nekonolock/config.toml` は dotfiles 管理。role は触らない。

主な変数は `defaults/main.yml` を参照。既定では次を行う。

- package を入れる
- `/etc/pam.d/nekonolock` を `auth` / `account` とも `login` include で置く
- `nekonolock-auth.socket` を user manager で enable + start する
- `~/.config/sway/config` の `exec swayidle ...` 行の lock コマンドを
  `nekonolock` に差し替える

swayidle の行を丸ごと置き換えるため、DPMS などの設定は
`nekonolock_swayidle_command` で調整する。role に任せない場合は
`nekonolock_manage_swayidle: false` にする。

- `nekonolock_sway_config` が無い場合は **失敗する**（自動ロックの設定漏れを
  黙って成功にしない）。sway を使わないホストでは
  `nekonolock_manage_swayidle: false` を明示する。
- `nekonolock_sway_config` が symlink の場合、実体が対象ユーザーの home 内に
  あるときだけ編集する。dotfiles の symlink はそのまま使える。home 外を指す
  symlink は root による home 外への書き込みになるため拒否する。
- `nekonolock_pam_service` / `nekonolock_pam_include` は
  ファイル名・PAM サービス名として安全な文字（英数字、`_`、`.`、`-`）だけを
  受け付ける。`/etc/pam.d` の外へのパスや改行の注入を防ぐ。
- `nekonolock_sway_config` は対象ユーザーとして編集する。root で play を
  実行する場合は、そのユーザーへ切り替えられる become（sudo など）が必要。
- `nekonolock_user` の既定は `ansible_user_id`。play 全体を `become: true` で
  実行すると事実上のユーザーが root になるため、デスクトップのユーザーを
  明示して渡す。
- 実行中の `swayidle` は設定を読み直さない。変更は次回の sway 起動
  （再ログイン）から有効になる。role の適用後に即時反映したい場合は、
  ユーザーセッションで swayidle を再起動する。

## 実機で必要な設定

- YubiKey: `scdaemon` を pcscd 経由で使う（`disable-ccid`）。`pam_pkcs11` /
  `opensc` / `pcsc-card-watcher` は外す（設計 §9）。
- `[auth] key` は実際の鍵の long key ID か fingerprint にする。
- パスワードを無効にするホストは `[auth] password = false` にする。
- `[pam] service` を変えた場合は、role の `nekonolock_pam_service` も合わせる。
