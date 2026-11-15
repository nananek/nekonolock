# 設計

## 1. 目的

Sway セッションのスクリーンロックを、小さく検証しやすい構成で作り直す。
要求は次の 4 点。

1. YubiKey で解除できる（PIN 入力）。
2. YubiKey が無いときはパスワードで解除できる。パスワードは設定で無効化できる。
3. カードアクセスの競合で挙動が不安定にならない。
4. sudo でも同じ YubiKey を使える（任意）。

## 2. 背景

移行前の構成は hyprlock + pam_pkcs11 (PIV) + pcsc-card-watcher で、次の問題を抱えていた。

- **PC/SC リーダの二重所有**: `scdaemon`（GnuPG。`disable-ccid` で pcscd 経由）と
  `pam_pkcs11`（opensc-pkcs11 で pcscd 経由）が、どちらも exclusive にカードを握る。
- 移行前の回避策は「ロック起動時と PAM auth 時に scdaemon を SIGKILL する」。
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
- sudo 用の PAM モジュール（任意）。
- pacman package と ansible role による配布。

**やらないこと**

- X11 / 他コンポジタ対応。
- IME 入力、通知連携、スクリーンセーバー的な演出。
- マルチユーザ対応（単一 UID 前提）。
- PIV 証明書による認証（PAM PIV は廃止する）。
- sudo 用 PAM モジュールの既定有効化（ホストごとに ansible で明示的に
  有効化する）。

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

+---------------------+        fork + setuid         +----------------------+
| sudo (PAM)          | ----------------------------> | gpg（実 uid）        |
| pam_nekonolock.so   |                               | ユーザーの gpg-agent  |
| - root 所有の設定    |                               | scdaemon / カード     |
| - root 側で署名検証  | <---------------------------- +----------------------+
+---------------------+        署名（チャレンジ）
```

UI は認証ロジックを持たない。UI と認証を分ける理由は次のとおり。

- 認証だけを CLI から叩いてテストできる。
- UI のクラッシュや再起動が認証状態に影響しない。
- sudo 用の PAM モジュールと署名・検証の実装（`crates/gpg`）を共有できる。

## 5. 認証方式

### 5.1 OpenPGP applet（第一手段）

カード上の秘密鍵でチャレンジに署名し、対応する公開鍵で検証する。

1. ヘルパが 32 byte のランダムチャレンジを作る。
2. gpg-agent の該当鍵の PIN キャッシュを `CLEAR_PASSPHRASE` で消す。
   gpg-agent は一度成功した PIN をキャッシュし、その間は間違った PIN でも
   署名を通すため、署名のたびに消す。
3. `gpg --local-user <key>! --detach-sign --pinentry-mode loopback
   --passphrase-fd 0` に PIN とチャレンジを渡す。
   （`!` は primary を固定するため必須。付けないと署名 subkey に解決され、
   別カードを要求する。）
4. 出力した署名を `gpg --verify` で検証し、チャレンジと一致することを確認する。
   `VALIDSIG` の fingerprint が設定鍵と一致し、`REVKEYSIG` / `KEYREVOKED` /
   `EXPKEYSIG` / `KEYEXPIRED` / `SIGEXPIRED` が無いことを確認する。

ロック画面と sudo は OpenPGP applet（scdaemon 経由）を使う。PIV
(`pam_pkcs11`) は SDDM / login / TTY 用に残すが、ロック画面の OpenPGP 経路とは
PAM を分け、ロック画面の password は pam_pkcs11 を通さない。

PIN の入力は `--pinentry-mode loopback` でヘルパが渡す。カード側の PIN
リトライ回数（3 回）を消費するのは「PIN モードとして送った入力」だけにする。

### 5.2 パスワード（第二手段、設定で無効化可能）

PAM の `/etc/pam.d/nekonolock` は password-only の chain を通す。`auth` は
`pam_faillock` + `pam_unix` だけにし、`system-auth` の `pam_pkcs11 sufficient`
を先に通さない。ロック画面のパスワードが PIV PIN としてカードに送られ、
PIV のリトライを消費するのを避けるため。`account` は `login` include のままで、
faillock とアカウントの期限切れを確認する。

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
  `YUBIKEY PIN` / `PASSWORD` のラベルを置く。
- 入力欄の枠と入力ドットもモード色にする。
- 色だけに頼らない（ラベル・ドット・枠の 3 点で区別できる）。
- 画面のコピー（左上ラベル、モード名、案内、状態メッセージ、日付、
  下端の操作ヒント）はすべて大文字で出す。時計だけは `clock_format` のとおり。
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
  パスワードモードへ切り替える。切り替え時は入力中の秘密を破棄し、自動送信しない。
  カードを挿し直しても PIN モードには戻さない。
- ロック開始時に `status` を問い合わせ、以降もカードがある間は 1 秒、無いと
  分かった後は 10 秒間隔でカードの有無を確認する。画面が純黒の間は問い合わせ
  ない。`gpg --card-status` は scdaemon とカードに触れるため、これ以上短くしない。
  応答が来るまではパスワードを既定にする（カードのリトライを消費しないため）。
  カードがあり `pin` が有効なら、最初の応答で PIN モードへ切り替える。
- 状態メッセージは既定フォントで描ける英語にする（`AUTH FAILED (1/5)`、
  `WAIT 5s`、`YUBIKEY ERROR`、`PAM ERROR` など）。
- `[ui] max_attempts` は連打抑止。認証失敗の直後に 1 秒、連続失敗が上限に
  達したら 5 秒のクールダウンを入れる。`0` で無効。恒久的な拒否はしない
  （正規の解除手段を塞がないため。[ADR-0017](decisions.md)）。
- 秘密（PIN / パスワード）はログに出さない。受信バッファは使用後に zeroize する。
  UI の入力欄も送信結果を受け取った時点で zeroize する。

### 5.5 sudo（任意）

sudo の YubiKey 認証は `pam_nekonolock.so` で行う。ロック画面の
`nekonolock-auth` socket には接続しない。UI 用ヘルパはユーザーが起動・
差し替えできるため、それを root への認証に使うと、同じ uid の攻撃者が
偽のヘルパで sudo を通せてしまう。共有するのは `crates/gpg` の
署名・検証コードだけにする。

1. sudo は実 uid（呼び出したユーザー）で PAM モジュールを呼ぶ。
2. モジュールは root 所有の `/etc/nekonolock/sudo.toml` と
   `/etc/nekonolock/sudo-key.asc` を読む。ユーザーの
   `~/.config/nekonolock/config.toml` は読まない。
3. `/run/user/<uid>` と home の所有者・モードを確認し、子プロセスの
   `gpg` を実 uid / gid に落として署名させる。PIN は PAM の会話
   （エコーなし）で受け取り、gpg の stdin に渡す。
4. 署名の検証は root 側で行う。一時 keyring に信頼した公開鍵だけを
   取り込み、`VALIDSIG` の fingerprint と失効・期限切れを確認する。
   PAM は実 uid = ユーザー、実効 uid = root で動くため、gpg が実 uid で
   ファイルを開くことに注意し、検証用の子プロセスは fork 後に実 uid も
   root へ切り替える。
5. 成功なら `PAM_SUCCESS`。カードが無い・PIN が違う・設定や鍵が読めない
   場合は `PAM_IGNORE` を返す。PAM スタックは `auth sufficient
   pam_nekonolock.so` を `auth include system-auth` の直前に置くので、
   パスワード認証へフォールバックする。

PIN の誤りはカード側のリトライ回数（3 回）だけを消費し、sudo の
`pam_faillock` には数えない。YubiKey を必須にしたいホストは、PAM の
`sufficient` を `required` に変えるのではなく、まずこの既定動作
（パスワードへのフォールバック）を理解してから別途検討する。

sudo の timestamp（既定 5 分）は PAM モジュールの外側で効く。timestamp が
有効な間は sudo が PAM を呼ばないため、YubiKey PIN も再要求されない。
YubiKey 成功もパスワード成功も同じ timestamp を更新する。毎回カードを要求する
には sudoers で `Defaults timestamp_timeout=0` にする必要がある。ただし現状は
失敗時に `PAM_IGNORE` でパスワードへフォールバックするため、これだけでは
YubiKey 必須にはならない。

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
label           = "NEKONOLOCK"      # 左上の短いラベル。描画時に大文字化する
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
  起動時に確認する。`date_format` と `label` は描画時に大文字化する。
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

### 7.1 sudo 用の設定

sudo 用の PAM モジュールは、ユーザーの設定ではなく root 所有の
`/etc/nekonolock/sudo.toml` を読む。検証用の公開鍵は
`/etc/nekonolock/sudo-key.asc`（ASCII armored）に置く。

```toml
[sudo]
enabled = true
key = "030130E2F156CD74"
public_key = "/etc/nekonolock/sudo-key.asc"
challenge_bytes = 32
timeout_sec = 30
users = []          # 空なら全ユーザー
```

- `nekonolock-auth` の `[auth] key` と同じ key ID / fingerprint を指定できる。
- 両ファイルは root 所有で、group / other から書き換えられないこと。
  `sudo.toml` は 0644、`sudo-key.asc` は 0644 を想定する。
- `enabled = false` ならモジュールは何もしない（既定）。
- `users` に名前を並べたときだけ、そのユーザーに限定する。
- 設定は ansible role の `nekonolock_sudo_auth` / `nekonolock_sudo_key` /
  `nekonolock_sudo_public_key` で配る。

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
  `fontdue` のビットマップを合成する。グリフはフォントの ascent / descent から
  求めた行の共通ベースラインに載せ、小文字と降下部の高さを保つ。
  シェーピングは不要（ASCII + 数字）。
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

- ロック画面と sudo の認証は OpenPGP applet（scdaemon 経由）に寄せる。
- PIV (`pam_pkcs11`) は SDDM / login / TTY 用に残す。ロック画面の password は
  `system-auth` の `pam_pkcs11` を通さない。
- `lock-release-scdaemon` のような scdaemon kill は廃止する。
- `pcsc-shared` は使わない（`disable-ccid` のまま）。

結果として、ロック解除・SSH 署名・git commit 署名が同じ scdaemon を共有し、
競合は「待つ」だけで解決する（pcscd が直列化する）。ロック中も同じ uid の
プロセスは署名できる。画面ロックをカードアクセスの境界にするのは未対応で、
別途設計する（[ADR-0029](decisions.md)）。

## 10. セキュリティ

- socket は同一 UID のみ。`SO_PEERCRED` の uid を検証する。
- 秘密はメモリ上でのみ扱い、ログ・環境変数・argv に出さない。
  `--passphrase-fd` を使い、argv 渡しにしない。
- パスワードは PAM の `pam_faillock` に委ねる。
- PIN の試行はカード側のリトライ回数が上限。UI の `max_attempts` は
  連打を抑えるだけで、PIN の自動再試行はしない。
- 署名の前に gpg-agent の PIN キャッシュを消す。キャッシュが温まっていると
  間違った PIN でも署名が通り、PIN を第二要素として扱えなくなる。
- **画面ロックは同一 uid のプロセスに対するカードアクセスの境界ではない
  （未対応）。** ロック中もユーザーの gpg-agent / scdaemon は動き続け、同じ
  uid のプロセスはカード署名を要求できる。ロック中に署名を止めるには、
  agent / scdaemon の停止か seat 単位の制御を別途設計する
  （[ADR-0029](decisions.md)）。
- OpenPGP の署名検証では、失効・期限切れの鍵（`REVKEYSIG` / `KEYREVOKED` /
  `EXPKEYSIG` / `KEYEXPIRED` / `SIGEXPIRED`）を拒否する。カードは失効を
  知らずに署名できるため、検証側で止める（[ADR-0016](decisions.md)）。
- 認証成功後は、UI が lock surface を破棄して終了する。
- ヘルパは認証結果以外を返さない（カードの詳細情報や PIN 残回数を返さない）。
- sudo 用 PAM モジュールはユーザーの `nekonolock-auth` socket を信頼しない。
  ユーザーが差し替えられるプロセスの応答を root の認証に使うと、同じ uid の
  攻撃者が偽のヘルパで sudo を通せてしまう。
- sudo 用の署名は実 uid の gpg-agent にさせるが、検証は root 側の一時
  keyring に取り込んだ root 所有の公開鍵だけで行う。`gpg` は絶対パス
  （`/usr/bin/gpg`）で起動し、環境変数は必要なものだけを設定する。
- sudo 用の設定と公開鍵は root 所有で、group / other から書き換えられない
  ことをモジュールが確認する。symlink は辿らず、開いた fd のメタデータを見る。

## 11. テスト方針

- 単体: プロトコルの serde 往復、設定の既定値と不正値、チャレンジ検証ロジック
  （署名器はモック）、入力状態（モード切替・文字種・長さ上限・クールダウン・
  カード抜去）、描画のレイアウト（フレーム太さ・帯の高さ・色の解釈）、時計の書式。
- sudo: `crates/pam` の設定検証（root 所有・モード・key/users/上限）、
  PIN の受理・拒否、カード無し・カードエラーのマッピングを単体テストする。
  PAM の会話関数と `pam_sm_authenticate` の export は実機の sudo で確認する。
- 統合: ヘルパを feature flag でモック PAM / モックカードにして、要求の受理・拒否、
  `kind` の不整合、タイムアウトを検証する。
  `crates/auth/tests/helper.rs` は実際にバイナリを起動して socket 越しに確認する。
  `crates/lock` の auth クライアントも、テスト内で socket を立てて往復を確認する。
- E2E: Sway 1.12 の headless 出力（pixman）で、2 出力のロック、
  出力追加・削除、誤 PIN 後の維持、正しい PIN / パスワードでの解除を確認する。
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
crates/gpg     # OpenPGP の署名・検証（認証ヘルパと PAM モジュールで共有）
crates/auth    # nekonolock-auth
crates/lock    # nekonolock (UI)
crates/pam     # pam_nekonolock.so（sudo）
packaging/     # PKGBUILD と ansible role のステージング
```

- pacman package `nekonolock`（[nekono] repo）。バイナリ 2 本、
  `/etc/pam.d/nekonolock`、systemd-user unit 2 つ（socket activation、
  `/usr/lib/systemd/user/`）、`/usr/lib/security/pam_nekonolock.so`、
  sudo 用設定の例、LICENSE を同梱する。
  依存は `gnupg` / `libxkbcommon` / `pam` / `ttf-dejavu`（既定フォント）。
  YubiKey を pcscd 経由で使うホスト向けに `pcsclite` / `ccid` を
  optdepends に置く。
- socket unit は `ListenSequentialPacket=` を使う。`ListenStream=` は
  `SOCK_STREAM` を作り、ヘルパの種別検証で起動できない（[ADR-0019](decisions.md)）。
- 認証ヘルパの user service は systemd sandbox を入れない。`UMask=0077` だけを
  残す。`CapabilityBoundingSet=` は user manager で
  `status=218/CAPABILITIES`、`RestrictAddressFamilies=AF_UNIX` などは PAM の
  setuid 経路 (`unix_chkpwd` / `pam_faillock`) を壊し、ロック画面のパスワードが
  通らなくなることを実機で確認した。PAM モジュールは sudo のプロセス内で
  動くため unit ではない。
- ansible role `nekonolock`（ansible-nekonodesk）: package 導入、
  `/etc/pam.d/nekonolock`、`nekonolock-auth.socket` の enable、
  `swayidle` の lock コマンド差し替え。unit は package の 1 か所だけに置き、
  role は enable する（[ADR-0019](decisions.md)）。`nekonolock_sudo_auth:
  true` のときだけ、sudo 用の設定・公開鍵を置き `/etc/pam.d/sudo` の
  `auth include system-auth` の前に `auth sufficient pam_nekonolock.so` を
  挿す（[ADR-0027](decisions.md)）。
- ユーザー設定（TOML）は dotfiles 管理。
- `packaging/` は移植用のステージング。pacman repo と ansible repo への
  移植手順は `packaging/README.md` に書く。

## 13. 実装状況

- `crates/proto` / `crates/auth`: `SOCK_SEQPACKET` + JSON、peer uid 検証、
  OpenPGP の loopback 署名と `VALIDSIG` 検証、`libpam.so.0` の `dlopen`。
- `crates/lock`: `ext-session-lock-v1` の全出力ロック、出力追加・削除への追従、
  `wl_keyboard` + xkbcommon の入力、CPU 描画、認証ヘルパとの連携。
- `crates/pam`: sudo 用 `pam_nekonolock.so`。既定は無効で、ansible の
  `nekonolock_sudo_auth` で明示的に有効化する。
- `packaging/`: pacman package と ansible role。検証は `spike/04-package-ansible.sh`
  と `spike/05-sudo-pam.sh`。
- UI: Ink / Paper テーマ、モード capsule、HiDPI（整数倍バッファ）、
  無操作時の純黒。`docs/images/` の画像は
  `cargo run -p nekonolock --example preview` で再生成する。

## 14. 残っている検討事項

- 時計の日本語表示に CJK フォントが必要かどうか（必要なら依存を追加）。
- fractional scale (1.25 / 1.5) の厳密な対応。`wl_output.scale` の整数倍で
  近似する（Sway の 1.75 は scale 2 で描く）。fractional-scale / viewporter は
  使わない。

## 15. 却下案

[docs/decisions.md](decisions.md) を参照。
