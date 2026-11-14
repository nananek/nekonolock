# 設計

## 1. 目的

Sway セッションのスクリーンロックを、小さく検証しやすい構成で作り直す。
要求は次の 3 点。

1. YubiKey で解除できる（PIN 入力）。
2. YubiKey が無いときはパスワードで解除できる。パスワードは設定で無効化できる。
3. カードアクセスの競合で挙動が不安定にならない。

## 2. 背景

現行構成は hyprlock + pam_pkcs11 (PIV) + pcsc-card-watcher で、次の問題を抱えている。

- **PC/SC リーダの二重所有**: `scdaemon`（GnuPG。`disable-ccid` で pcscd 経由）と
  `pam_pkcs11`（opensc-pkcs11 で pcscd 経由）が、どちらも exclusive にカードを握る。
- 現行の回避策は「ロック起動時と PAM auth 時に scdaemon を SIGKILL する」。
  これはタイミング依存で、SSH や commit の署名要求と重なると失敗する。
- hyprlock は Hyprland 向けの実装で、Sway 上では入力の扱いが不安定。
  実際に `pam_unix(hyprlock:auth): authentication failure` が、正しい
  パスワードでも発生する（PAM を直接叩くと成功する）。原因は hyprlock が
  PAM に渡す入力文字列側。
- `pcsc-shared` は過去の実測で不採用（pcscd の同時接続で不安定）。

## 3. スコープ

**やること**

- Sway 用のロック画面（`ext-session-lock-v1`）と、独立した認証ヘルパ。
- OpenPGP applet によるチャレンジ署名認証。
- PAM によるパスワード認証（設定で無効化可能）。
- pacman package と ansible role による配布。
- まず ayaka に適用し、安定後に inspiron へ展開する。

**やらないこと**

- X11 / 他コンポジタ対応。
- IME 入力、通知連携、スクリーンセーバー的な演出。
- マルチユーザ対応（単一 UID 前提）。
- PIV 証明書による認証（PAM PIV は廃止する）。
- sudo の YubiKey 認証（当面はパスワードに戻す。必要になれば別 phase）。

## 4. アーキテクチャ

```
+---------------------+        Unix socket        +----------------------+
| nekonolock (UI)     |  SOCK_SEQPACKET, JSON     | nekonolock-auth      |
| - ext-session-lock  | <-----------------------> | - 要求の検証          |
| - 全出力に surface  |                           | - OpenPGP 署名検証    |
| - 入力欄 / 時計     |                           | - PAM パスワード認証  |
| - モード表示        |                           | - 失敗回数の管理      |
+---------------------+                           +----------+-----------+
                                                             |
                                        gpg-agent / scdaemon / pcscd
                                        （カードを持つのはここだけ）
```

UI は認証ロジックを持たない。UI と認証を分ける理由は次のとおり。

- 認証だけを CLI から叩いてテストできる。
- UI のクラッシュや再起動が認証状態に影響しない。
- 将来 sudo 用の PAM モジュールから同じヘルパを再利用できる。

## 5. 認証方式

### 5.1 OpenPGP applet（第一手段）

カード上の秘密鍵でチャレンジに署名し、対応する公開鍵で検証する。

1. ヘルパが 32 byte のランダムチャレンジを作る。
2. `gpg --local-user <key>! --detach-sign --pinentry-mode loopback
   --passphrase-fd 0` に PIN とチャレンジを渡す。
   （`!` は primary を固定するため必須。付けないと署名 subkey に解決され、
   別カードを要求する。）
3. 出力した署名を `gpg --verify` で検証し、チャレンジと一致することを確認する。

カードを持つのは scdaemon だけになるため、pcscd のセッション競合は起きない。
`pam_pkcs11` / `opensc` / `pcsc-card-watcher` は ayaka から外す。

PIN の入力は `--pinentry-mode loopback` でヘルパが渡す。カード側の PIN
リトライ回数（3 回）を消費するのは「PIN モードとして送った入力」だけにする。

### 5.2 パスワード（第二手段、設定で無効化可能）

PAM の `/etc/pam.d/nekonolock`（`auth include login`）を通す。
`system-auth` の `pam_faillock` がそのまま効くので、総当たりは
アカウントロックで抑止される。

### 5.3 モードの決定（PIN リトライ保護）

カードの PIN は 3 回でブロックされるため、「パスワードを PIN として
試す」設計にしてはならない。UI が明示的にモードを持ち、ヘルパは要求の
`kind` をそのまま尊重する。

- 既定モード: 対象カードが挿さっていれば `pin`、無ければ `password`。
- UI 上で `Tab` により `pin` / `password` を切り替える。
- 設定で片方を無効化できる。両方無効は起動時にエラーにする。
- ヘルパは `kind` と設定の不整合（例: `password` 無効なのに `password` 要求）を
  拒否する。

**モード表示は「誤入力防止の主要 UI」として扱う。** 取り違えると
パスワードが PIN として送られ、カードのリトライ回数を消費するため、
控えめなインジケータでは不十分。

- 画面の外周に 12 px の枠を引き、モード色で塗る。
- 画面上部に高さ 20% 程度の帯を敷き、モード名を大きく出す。
  - `pin`: 藍 + 黄。ラベルは `YubiKey PIN` と「YubiKey の PIN」。
  - `password`: 臙脂 + 白。ラベルは `Password` と「パスワード」。
- 入力ドットもモード色に合わせる。
- 色だけに頼らない（文字と枠の太さでも区別できる）。
- 切替は即時反映し、入力中の文字列は破棄する（モード混在を防ぐ）。
- 点滅・明滅はしない（眩しさと混乱を避ける）。

### 5.4 失敗時の扱い

- 認証失敗はロックを維持する（fail closed）。失敗回数と最終メッセージを UI に出す。
- ヘルパに接続できない場合は「認証ヘルパに接続できない」を表示し、ロックは維持。
  ヘルパは socket activation で再起動させる。
- カードが抜けた場合はその旨を表示し、`password` が有効ならパスワードへ誘導する。
- 秘密（PIN / パスワード）はログに出さない。受信バッファは使用後に zeroize する。

## 6. プロトコル

- `$XDG_RUNTIME_DIR/nekonolock/auth.sock`、`SOCK_SEQPACKET`。
- ディレクトリ 0700、socket 0600。同一 UID 以外は接続できない前提にする
  （加えて `SO_PEERCRED` で uid を検証する）。
- 1 接続 1 要求。応答後にヘルパ側から close する。
- JSON 1 オブジェクト。`v` でバージョンを固定する。

```json
{"v":1,"op":"status"}
{"v":1,"ok":true,"card":true,"methods":["pin","password"]}

{"v":1,"op":"unlock","kind":"pin","secret":"123456"}
{"v":1,"ok":true,"method":"openpgp"}

{"v":1,"op":"unlock","kind":"password","secret":"..."}
{"v":1,"ok":false,"reason":"auth-failed"}
```

`reason` の語彙: `auth-failed` / `no-card` / `gpg-error` / `pam-error` /
`pin-disabled` / `password-disabled` / `bad-request`。

## 7. 設定

`~/.config/nekonolock/config.toml`（dotfiles 管理）。

```toml
[ui]
clock_format    = "%Y-%m-%d (%a) %H:%M"
font            = "/usr/share/fonts/TTF/DejaVuSansMono.ttf"
background      = "#000000"
foreground      = "#eeeeee"
dim             = "#888888"
max_attempts    = 5       # UI 側の連打抑止 (0 で無効)
frame_width     = 12      # モード色フレームの太さ
pin_accent      = "#1b2a6b"   # 藍
pin_on_accent   = "#ffd400"   # 黄
password_accent     = "#7a1024"  # 臙脂
password_on_accent  = "#ffffff"

[auth]
openpgp  = true
password = true
key      = "030130E2F156CD74!"
challenge_bytes = 32
timeout_sec     = 60

[pam]
service = "nekonolock"
```

- 既定は `openpgp = true` / `password = true`。
- `password = false` で YubiKey 必須運用にできる。
- `openpgp = false` でパスワードのみにもできる。
- 両方 `false` は起動時に拒否する。

## 8. Wayland 実装方針

- `ext_session_lock_manager_v1`（Sway 1.12 で確認済み）を使う。
  コンポジタが他クライアントの描画を止めるため、layer-shell の重ね合わせで
  代用するより確実。
- 全出力に `ext_session_lock_surface_v1` を張る。`wl_output` の追加・削除に追従する。
- 入力は `wl_keyboard`。IME は使わない。修飾キーは compositor のキーマップに従う。
- 描画は `wl_shm` のバッファに CPU で書く。時計・入力ドット・メッセージだけなので
  GPU は使わない。
- 画面構成は 3 層。外周のモード色フレーム（12 px）、上部のモード帯（高さ 20%、
  モード名を大きく表示）、中央の時計と入力ドット。
- モード色は `[ui] pin_accent` / `password_accent` で設定可能にする。
  既定は pin = 藍 + 黄、password = 臙脂 + 白。
- フォントは `fontdue` でラスタライズする。シェーピングは不要（ASCII + 数字）。
  CJK を出す予定はないが、時計の曜日を日本語にする場合は設定でフォントを差し替える。
- 依存は `smithay-client-toolkit`、`wayland-client`、`wayland-protocols`、
  `memmap2`、`fontdue`、`serde` / `serde_json`、`toml`、`nix`、`pam-client`。

## 9. カード排他の解消

変更点は「カードを持つプロセスを 1 つにする」ことに尽きる。

- 認証は OpenPGP applet（scdaemon 経由）に統一する。
- `pam_pkcs11`、`opensc`、`ykman` の常駐、`pam_yubikey` role の PAM 注入を
  ayaka から外す。
- `lock-release-scdaemon` のような scdaemon kill は廃止する。
- `pcsc-shared` は使わない（`disable-ccid` のまま）。

結果として、ロック解除・SSH 署名・git commit 署名が同じ scdaemon を共有し、
競合は「待つ」だけで解決する（pcscd が直列化する）。

## 10. セキュリティ

- socket は同一 UID のみ。`SO_PEERCRED` の uid を検証する。
- 秘密はメモリ上でのみ扱い、ログ・環境変数・argv に出さない。
  `--passphrase-fd` を使い、argv 渡しにしない。
- パスワードは PAM の `pam_faillock` に委ねる。
- PIN の試行はカード側のリトライ回数が上限。UI の `max_attempts` は
  連打を抑えるだけで、PIN の自動再試行はしない。
- 認証成功後は、UI が lock surface を破棄して終了する。
- ヘルパは認証結果以外を返さない（カードの詳細情報や PIN 残回数を返さない）。

## 11. テスト方針

- 単体: プロトコルの serde 往復、設定の既定値と不正値、チャレンジ検証ロジック
  （署名器はモック）。
- 統合: ヘルパを feature flag でモック PAM / モックカードにして、要求の受理・拒否、
  `kind` の不整合、タイムアウトを検証する。
- E2E: ayaka 実機で手動チェックリスト。
  - YubiKey 挿入 → PIN で解除
  - YubiKey 抜去 → パスワードで解除
  - パスワード無効設定で PIN のみ
  - 誤 PIN 2 回 → 正しい PIN で解除できる（リトライを消費しない経路の確認）
  - ロック中に SSH 署名を走らせても解除できる（scdaemon 競合の回帰確認）
- CI: `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test`。

## 12. 配布

- リポジトリ構成（Cargo workspace）:

```
crates/proto   # 要求/応答の型と socket ヘルパ
crates/auth    # nekonolock-auth
crates/lock    # nekonolock (UI)
packaging/     # PKGBUILD (nekono-pacman-distribution へ移植)
```

- pacman package `nekonolock`（[nekono] repo）。バイナリ 2 本と
  `/etc/pam.d/nekonolock`、systemd-user unit 2 つ（socket activation）。
- ansible role `nekonolock`（ansible-nekonodesk）: package 導入、
  `/etc/pam.d/nekonolock`、`/etc/systemd/user/nekonolock-auth.{socket,service}`、
  `swayidle` の lock コマンド差し替え。
- ユーザー設定（TOML）は dotfiles 管理。systemd-user unit は ansible 管理。

## 13. ロードマップ

- [ ] Phase 0: 設計（本ドキュメント）
- [ ] Phase 1: スパイク
  - `gpg --pinentry-mode loopback --passphrase-fd` でカード鍵に署名できるか
  - Sway の `ext-session-lock-v1` で全出力ロックできるか
- [ ] Phase 2: `crates/proto` と `crates/auth`（OpenPGP + PAM）+ テスト
- [ ] Phase 3: `crates/lock`（単一出力 → 全出力、入力、描画）
- [ ] Phase 4: packaging と ansible role
- [ ] Phase 5: ayaka 適用（pam_pkcs11 / hyprlock 廃止、swayidle から起動）
- [ ] Phase 6: inspiron 適用（当面 swayidle 無効のまま）
- [ ] Phase 7（任意）: sudo 用 PAM モジュール（同じヘルパを再利用）

## 14. 未確定事項

- `gpg --pinentry-mode loopback` がカード鍵の PIN を scdaemon へ正しく渡せるか。
  不可なら gpg-agent へ Assuan で直接 `SIGKEY` / `PWD` を送る実装に切り替える。
- Sway 1.12 の `ext-session-lock-v1` で、ロック中の出力追加・削除の挙動。
- 時計の日本語表示に CJK フォントが必要かどうか（必要なら依存を追加）。

## 15. 却下案

[docs/decisions.md](decisions.md) を参照。
