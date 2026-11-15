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
   `VALIDSIG` の fingerprint が設定鍵と一致し、`REVKEYSIG` / `KEYREVOKED` /
   `EXPKEYSIG` / `KEYEXPIRED` / `SIGEXPIRED` が無いことを確認する。

カードを持つのは scdaemon だけになるため、pcscd のセッション競合は起きない。
`pam_pkcs11` / `opensc` / `pcsc-card-watcher` は対象ホストから外す。

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

- 画面上端にモード色の細い accent line を引く。
- 日付の下にモード capsule（丸い pill）を出す。モード色のドットと
  `YubiKey PIN` / `Password` のラベルを置く。
- 入力欄の枠と入力ドットもモード色にする。
- 色だけに頼らない（ラベル・ドット・枠の 3 点で区別できる）。
- 切替は即時反映し、入力中の文字列は破棄する（モード混乱を防ぐ）。
- 点滅・明滅はしない（眩しさと混乱を避ける）。

既定テーマは濃いチャコールの **Ink**。明るい **Paper** は同じ UI の色違いで、
`[ui]` の色と `clock_font` を差し替えて使う（§7）。時計は
`DejaVuSans-ExtraLight`、本文は `DejaVuSans`、Paper の時計は `DejaVuSerif` を
既定にする。いずれも ASCII 中心なので、ラベルとメッセージは英語で出す。
日本語を出す場合は `[ui] font` を CJK 対応フォントへ差し替える。
フォントを読めない場合は、ロックを先に取得したうえでテキストなしの
accent line・capsule・入力欄でロックを維持する（[ADR-0018](decisions.md)）。
package は `ttf-dejavu` を依存に入れる。

### 5.4 失敗時の扱い

- 認証失敗はロックを維持する（fail closed）。失敗回数と最終メッセージを UI に出す。
- ヘルパに接続できない場合は `AUTH HELPER UNAVAILABLE` を表示し、ロックは維持。
  ヘルパは socket activation で再起動させる。
- ヘルパは要求の読み取りに `timeout_sec` を使う。時間内に要求が来なければ
  応答せず接続を閉じる（UI は接続失敗として扱う）。
- カードが抜けた場合は `YUBIKEY NOT FOUND` を表示し、`password` が有効なら
  パスワードモードへ切り替える。
- ロック開始時に `status` を 1 回問い合わせ、カードの有無と有効な方式を反映する。
  応答が来るまではパスワードを既定にする（カードのリトライを消費しないため）。
  カードがあり `pin` が有効なら、その応答で PIN モードへ切り替える。
- 状態メッセージは既定フォントで描ける英語にする（`AUTH FAILED (1/5)`、
  `WAIT 5s`、`YUBIKEY ERROR`、`PAM ERROR` など）。
- `[ui] max_attempts` は連打抑止。認証失敗の直後に 1 秒、連続失敗が上限に
  達したら 5 秒のクールダウンを入れる。`0` で無効。恒久的な拒否はしない
  （正規の解除手段を塞がないため。[ADR-0017](decisions.md)）。
- 秘密（PIN / パスワード）はログに出さない。受信バッファは使用後に zeroize する。
  UI の入力欄も送信結果を受け取った時点で zeroize する。

## 6. プロトコル

- `$XDG_RUNTIME_DIR/nekonolock/auth.sock`、`SOCK_SEQPACKET`。
- ディレクトリ 0700、socket 0600。同一 UID 以外は接続できない前提にする
  （加えて `SO_PEERCRED` で uid を検証する）。
- 1 接続 1 要求。応答後にヘルパ側から close する。
- JSON 1 オブジェクト。`v` でバージョンを固定する。
- 1 メッセージは 4096 byte まで。秘密は 512 byte まで。超えた要求と
  `v` の不一致は `bad-request` としてロックを維持する。
- systemd socket activation で fd を引き取る場合も、fd が `SOCK_SEQPACKET`
  であることを検証する。`ListenStream=` などの設定ミスをそのまま使わない。

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

既定（Ink テーマ）:

```toml
[ui]
clock_format    = "%H:%M"
date_format     = "%A, %-d %B %Y"   # 描画時に大文字化する
font            = "/usr/share/fonts/TTF/DejaVuSans.ttf"
clock_font      = "/usr/share/fonts/TTF/DejaVuSans-ExtraLight.ttf"
label           = "NEKONOLOCK"      # 左上の短いラベル。空なら描かない
background      = "#15181B"
background_bottom = "#0B0D0F"
foreground      = "#F2EFE7"
dim             = "#9B9E9C"
faint           = "#6D7274"
field_background = "#1B1F22"
pin_accent      = "#8FC7B8"
password_accent = "#E0A18D"
max_attempts    = 5                 # UI 側の連打抑止 (0 で無効)
blank_after_sec = 60                # 無入力で純黒にするまで (0 で無効)

[auth]
openpgp  = true
password = true
key      = "030130E2F156CD74!"
challenge_bytes = 32
timeout_sec     = 60

[pam]
service = "nekonolock"
```

Paper テーマにする場合は `[ui]` の色と時計フォントだけを差し替える。

```toml
[ui]
font        = "/usr/share/fonts/TTF/DejaVuSans.ttf"
clock_font  = "/usr/share/fonts/TTF/DejaVuSerif.ttf"
background  = "#F8F4EC"
background_bottom = "#EDE6DA"
foreground  = "#1F1D1A"
dim         = "#7B7265"
faint       = "#A79C8C"
field_background = "#FFFDF8"
pin_accent  = "#2F5D62"
password_accent = "#A34A3A"
```

- 既定は `openpgp = true` / `password = true`。
- `password = false` で YubiKey 必須運用にできる。
- `openpgp = false` でパスワードのみにもできる。
- 両方 `false` は起動時に拒否する。
- `clock_format` / `date_format` は空でなく `chrono` が解釈できることを
  起動時に確認する。`date_format` は描画時に大文字化する。
- `blank_after_sec` はロック中の無入力時間。経過すると画面を純黒にし、キー入力で
  戻す。`0` で無効。認証待ちの間は黒くしない。
- `max_attempts` は連打抑止のクールダウン（§5.4）。
- `key` は 16 桁の long key ID か 40 桁の fingerprint（16 進）だけを許可する。
  可能なら 40 桁の fingerprint を使う。照合は key ID なら fingerprint の
  末尾一致、fingerprint なら完全一致にする。
- 認証ヘルパは `[auth]` と `[pam]` だけを検証する。`[ui]` の色やフォントの
  誤りでロック解除できなくなる（fail closed のまま締め出される）のを避けるため。
  UI は `[ui]` を独立に解釈し、`[auth]` / `[pam]` の型・未知項目の誤りは
  既定値に落としてロックを維持する。トップレベルの未知の項目は両方が拒否する。

## 8. Wayland 実装方針

- `ext_session_lock_manager_v1`（Sway 1.12 で確認済み）を使う。
  コンポジタが他クライアントの描画を止めるため、layer-shell の重ね合わせで
  代用するより確実。
- Wayland に接続したら、設定・フォント・socket パスを読む前に `lock()` を呼ぶ。
  その後 2 秒だけフォールバック描画でサーフェスを出し、設定とフォントを読む。
  読み込みに失敗しても既定値とテキストなし描画でロックを維持する
  （[ADR-0018](decisions.md)）。
- 全出力に `ext_session_lock_surface_v1` を張る。`wl_output` の追加・削除に追従する。
- 入力は `wl_keyboard`。IME は使わない。修飾キーは compositor のキーマップに従う。
  `smithay-client-toolkit` の `xkbcommon` feature を使い、キーリピートは UI の
  tick で進める（Enter / Tab / Esc は繰り返さない）。
- 描画は `wl_shm` のバッファに CPU で書く。時計・入力ドット・メッセージだけなので
  GPU は使わない。バッファは `slot::SlotPool` に任せ、compositor の release 後に
  再利用する。
- 画面構成は上から、上端の accent line、左上ラベル / 右上 `LOCKED`、時計、
  日付、モード capsule、入力欄、案内メッセージ、下端の操作ヒント。
  外周フレームと上部の色帯は使わない。
- 角丸矩形と円は SDF のカバレッジでアンチエイリアスする。テキストは
  `fontdue` のビットマップを合成する。シェーピングは不要（ASCII + 数字）。
  CJK を出す予定はないが、時計の曜日を日本語にする場合は設定でフォントを差し替える。
- モード色は `[ui] pin_accent` / `password_accent` で設定可能にする。
  capsule の下地は accent を背景に 16% 混ぜて作る。
- **HiDPI**: `ext_session_lock_surface_v1` の configure は論理サイズなので、
  出力の `wl_output.scale`（Sway の 1.75 では 2）を掛けたバッファを確保し、
  `wl_surface.set_buffer_scale(scale)` で論理サイズに戻す。レイアウトは
  バッファの幅・高さに対する比率で置くため、倍率が変わっても同じ形になる。
  fractional-scale / viewporter は使わない（整数倍で近似する）。
- **焼き付き対策**: `blank_after_sec` の無入力で画面全体を純黒にする。
  キー入力で UI を戻し、復帰に使った最初のキーは入力として消費する。
  DPMS / バックライトの制御はしない。
- 依存は `smithay-client-toolkit`（`calloop` + `xkbcommon`）、`wayland-client`、
  `wayland-protocols`、`memmap2`、`fontdue`、`chrono`、`serde` / `serde_json`、
  `toml`、`nix`、`zeroize`。
  認証ヘルパは PAM を `libloading` で `dlopen` し、乱数は `getrandom`、
  gpg 用の一時ファイルは `tempfile` を使う（[ADR-0013](decisions.md)）。
- `xkbcommon` は実行時にも必要（Arch: `libxkbcommon`、Debian/Ubuntu:
  `libxkbcommon-dev` はビルド時、実行時は `libxkbcommon0`）。

## 9. カード排他の解消

変更点は「カードを持つプロセスを 1 つにする」ことに尽きる。

- 認証は OpenPGP applet（scdaemon 経由）に統一する。
- `pam_pkcs11`、`opensc`、`ykman` の常駐、`pam_yubikey` role の PAM 注入を
  対象ホストから外す。
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
- OpenPGP の署名検証では、失効・期限切れの鍵（`REVKEYSIG` / `KEYREVOKED` /
  `EXPKEYSIG` / `KEYEXPIRED` / `SIGEXPIRED`）を拒否する。カードは失効を
  知らずに署名できるため、検証側で止める（[ADR-0016](decisions.md)）。
- 認証成功後は、UI が lock surface を破棄して終了する。
- ヘルパは認証結果以外を返さない（カードの詳細情報や PIN 残回数を返さない）。

## 11. テスト方針

- 単体: プロトコルの serde 往復、設定の既定値と不正値、チャレンジ検証ロジック
  （署名器はモック）、入力状態（モード切替・文字種・長さ上限・クールダウン）、
  描画のレイアウト（フレーム太さ・帯の高さ・色の解釈）、時計の書式。
- 統合: ヘルパを feature flag でモック PAM / モックカードにして、要求の受理・拒否、
  `kind` の不整合、タイムアウトを検証する。
  `crates/auth/tests/helper.rs` は実際にバイナリを起動して socket 越しに確認する。
  `crates/lock` の auth クライアントも、テスト内で socket を立てて往復を確認する。
- E2E (Phase 3): Sway 1.12 の headless 出力（pixman）で、2 出力のロック、
  出力追加・削除、誤 PIN 後の維持、正しい PIN / パスワードでの解除を確認済み。
- E2E: 実機で手動チェックリスト。
  - YubiKey 挿入 → PIN で解除
  - YubiKey 抜去 → パスワードで解除
  - パスワード無効設定で PIN のみ
  - 誤 PIN 2 回 → 正しい PIN で解除できる（リトライを消費しない経路の確認）
  - ロック中に SSH 署名を走らせても解除できる（scdaemon 競合の回帰確認）
- CI: `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test`。
  モックを使う結合テストは `mock-card,mock-pam` を付けて別に実行する。
  モック feature を付けた release ビルドは `compile_error!` で拒否する
  （[ADR-0015](decisions.md)）。

## 12. 配布

- リポジトリ構成（Cargo workspace）:

```
crates/proto   # 要求/応答の型と socket ヘルパ
crates/auth    # nekonolock-auth
crates/lock    # nekonolock (UI)
packaging/     # PKGBUILD と ansible role のステージング
```

- pacman package `nekonolock`（[nekono] repo）。バイナリ 2 本、
  `/etc/pam.d/nekonolock`、systemd-user unit 2 つ（socket activation、
  `/usr/lib/systemd/user/`）、LICENSE を同梱する。
  依存は `gnupg` / `libxkbcommon` / `pam` / `ttf-dejavu`（既定フォント）。
  YubiKey を pcscd 経由で使うホスト向けに `pcsclite` / `ccid` を
  optdepends に置く。
- socket unit は `ListenSequentialPacket=` を使う。`ListenStream=` は
  `SOCK_STREAM` を作り、ヘルパの種別検証で起動できない（[ADR-0019](decisions.md)）。
- ansible role `nekonolock`（ansible-nekonodesk）: package 導入、
  `/etc/pam.d/nekonolock`、`nekonolock-auth.socket` の enable、
  `swayidle` の lock コマンド差し替え。unit は package の 1 か所だけに置き、
  role は enable する（[ADR-0019](decisions.md)）。
- ユーザー設定（TOML）は dotfiles 管理。
- `packaging/` は移植用のステージング。pacman repo と ansible repo への
  移植手順は `packaging/README.md` に書く。

## 13. ロードマップ

- [x] Phase 0: 設計（本ドキュメント）
- [ ] Phase 1: スパイク
  - [x] `gpg --pinentry-mode loopback --passphrase-fd` でカード鍵に署名できるか
        （2026-11-14 実測: `spike/01-gpg-card-loopback.sh` で sign/verify とも OK。
        EDDSA 483DC691DF9F29327EA106BD030130E2F156CD74。この方式で実装する）
  - [x] Sway 1.12 の `ext-session-lock-v1` で全出力ロックできるか
        （2026-11-14 実測: Arch Linux + Sway 1.12（wlroots headless / pixman）の
        VM で `crates/lock` のプロトタイプを動かした。2 出力にロックサーフェスを
        張り、ロック中の出力追加（HEADLESS-3）にも追従し、キー入力で解除して
        終了コード 0 で終了した。ログは `locked` → `lock-surface: created
        HEADLESS-2/1` → `1280x720` → `created HEADLESS-3` → `key-pressed` →
        `unlocked` → `done`。出力の削除だけは未確認。
        再現手順は `spike/02-ext-session-lock-vm.sh`（Docker + KVM が必要）。
        この測定は `--features insecure-test-unlock` を付けた検証用ビルドで
        行った。既定ビルドは Phase 3 までキー入力を無視していた（fail-closed）。
        Phase 3 で認証を実装し、`insecure-test-unlock` feature は削除した。
        `spike/02` は現在、ロックサーフェス・出力追加・認証なしのキー入力で
        解除されないことを確認する回帰スクリプトになっている。解除までの
        E2E は `spike/03-lock-ui-sway.sh`。）
- [x] Phase 2: `crates/proto` と `crates/auth`（OpenPGP + PAM）+ テスト
  （2026-11-15 実装。`SOCK_SEQPACKET` + JSON、peer uid 検証、0700/0600、
  gpg の loopback 署名と VALIDSIG 検証、`libpam.so.0` の `dlopen`、
  `mock-card` / `mock-pam` による結合テスト。gpg の署名は EDDSA の実カードで
  Phase 1 に確認済みの方式を使う。PAM 実機の `/etc/pam.d/nekonolock` は Phase 4。）
- [x] Phase 3: `crates/lock`（全出力、入力、描画、認証連携）
  （2026-11-15 実装。`ext-session-lock-v1` の全出力ロックと出力追加・削除への
  追従、`wl_keyboard` + xkbcommon の入力、accent line / モード capsule / 時計 /
  入力欄 / メッセージの CPU 描画、`nekonolock-auth` への status / unlock
  要求（別スレッド）、失敗時のロック維持。`insecure-test-unlock` は削除した。
  設定・フォントの読み込みより先にロックを取得し、読み込み失敗時はフォールバック
  描画でロックを維持する。
  Sway 1.12 headless（pixman）で 2 出力、出力追加・削除、誤 PIN 後の維持、
  正しい PIN / パスワードでの解除を確認。UI の自動テストは
  `cargo test --workspace`。再現手順は `spike/03-lock-ui-sway.sh`
  （Docker + モック認証ヘルパ）。）
- [x] Phase 4: packaging と ansible role
  （2026-11-15 実装。`packaging/PKGBUILD`、`/etc/pam.d/nekonolock`、
  `ListenSequentialPacket=` の systemd user unit 2 本、ansible role
  `nekonolock`。role は package 導入、PAM 配置、socket の enable、
  swayidle の差し替えを行う。検証は `spike/04-package-ansible.sh`
  （Arch Linux コンテナで makepkg / pacman -U / ansible-playbook）。
  v0.1.0 tag 後は tag archive を pin している（[ADR-0021](decisions.md)）。）
- [x] UI: Ink テーマ、モード capsule、HiDPI（integer buffer scale）、
  無操作時の純黒
  （2026-11-15。`docs/images/` の画像は
  `cargo run -p nekonolock --example preview` で再生成する。）
- [ ] Phase 7（任意）: sudo 用 PAM モジュール（同じヘルパを再利用）

## 14. 未確定事項

- ~~`gpg --pinentry-mode loopback` がカード鍵の PIN を scdaemon へ正しく渡せるか。~~
  **解決済み (2026-11-14)**: `spike/01-gpg-card-loopback.sh` で
  `--pinentry-mode loopback --passphrase-fd` による署名と検証が成功した。
  実装はこの方式を前提にする（Assuan 直叩きは不要）。
- Sway 1.12 の `ext-session-lock-v1` で、ロック中の出力削除の挙動。
  2026-11-14 時点では未確認だったが、Phase 3 で `swaymsg output HEADLESS-3
  unplug` により削除へ追従することを確認した。
- 時計の日本語表示に CJK フォントが必要かどうか（必要なら依存を追加）。
- fractional scale (1.25 / 1.5) の厳密な対応。`wl_output.scale` の整数倍で
  近似する（Sway の 1.75 は scale 2 で描く）。fractional-scale / viewporter は
  使わない。

## 15. 却下案

[docs/decisions.md](decisions.md) を参照。
