# 決定と却下案

設計上の決定を、理由と却下した案つきで記録する。実装中に判断を変えたら
このファイルに追記する（過去の記述は消さない）。

## ADR-0001: 実装言語は Rust

- 決定: Rust（edition 2024、`rustc` 1.99 で確認）。
- 理由: Wayland クライアント（`smithay-client-toolkit`）、PAM、Unix socket の
  いずれも実績のある crate がある。UI とロック解除を扱うコードで
  メモリ安全性を確保したい。
- 却下: C / C++ — 依存は減るが、入力処理とバッファ管理のリスクが大きい。
- 却下: Go — PAM と Wayland の binding が cgo 前提になり、静的バイナリの
  利点が消える。

## ADR-0002: ロック画面は自前実装する

- 決定: `nekonolock`（自前 UI）+ `nekonolock-auth`（認証ヘルパ）の 2 プロセス。
- 理由: 現行 hyprlock は Hyprland 向けで、Sway 上では PAM に渡す入力が
  壊れる（正しいパスワードでも `pam_unix` が失敗する）。UI を小さく自前で持ち、
  認証を分離すれば、どちらに問題があるか切り分けられる。
- 却下: swaylock + 自前 PAM モジュール — UI は堅いが、認証以外の不満
  （表示・モード切替・失敗時の挙動）を制御できない。工数は小さいので、
  行き詰まったときの退避先としては残す。

## ADR-0003: YubiKey 認証は OpenPGP applet に一本化する

- 決定: scdaemon 経由のチャレンジ署名で解除する。PIV / pam_pkcs11 /
  opensc / pcsc-card-watcher は対象ホストから外す。
- 理由: 競合の原因は「PC/SC リーダを握るクライアントが 2 系統ある」こと。
  認証を scdaemon 側に寄せれば、カードを持つプロセスが 1 つになり、
  競合が構造的に消える。SSH 署名・git commit 署名と同じ経路になる。
- 却下: PIV + リースブローカー — 現行の PIV 証明書は維持できるが、
  排他デーモンを別途作る必要があり、scdaemon 側の挙動は結局ブラックボックス。
- 却下: `pcsc-shared` — 2026-05-13 の実測で不採用。pcscd の同時接続で
  scdaemon が不安定になる。
- 変更 (2026-11-15): PIV (`pam_pkcs11`) は SDDM / login / TTY 用に残す。
  ロック画面と sudo は OpenPGP、SDDM / login は PIV と使い分ける。

## ADR-0004: 認証は別プロセスに分離し、Unix socket で受ける

- 決定: UI は秘密を受け取ってヘルパに渡すだけ。認証判定は
  `nekonolock-auth` が行う。
- 理由: 認証だけを CLI から検証できる。UI のクラッシュが認証状態を壊さない。
  将来 sudo 用 PAM モジュールから同じロジックを再利用できる。
- 却下: 単一バイナリで PAM / gpg を直接呼ぶ — テストしづらく、
  UI の再起動で認証状態（試行回数など）が消える。

## ADR-0005: `ext-session-lock-v1` を使う

- 決定: `ext_session_lock_manager_v1` でロックする。
- 理由: Sway 1.12 が対応している。コンポジタが他クライアントの描画を止めるため、
  layer-shell を全面に重ねる方式より確実。
- 却下: `zwlr_layer_shell_v1` のみで全面を覆う — 通知や他 surface が
  上に出る余地があり、ロックとして弱い。

## ADR-0006: パスワード解除は既定で有効、設定で無効化できる

- 決定: `[auth] password = true` を既定。`false` で YubiKey 必須にできる。
- 理由: YubiKey を挿し忘れたときの締め出しを避ける。一方で、
  持ち出し端末では PIN のみにしたいという要求もある。
- 補足: 両方 `false` は起動時に拒否する。

## ADR-0007: 描画は CPU（wl_shm + fontdue）、GPU は使わない

- 決定: `wl_shm` のバッファに直接描画する。フォントは `fontdue`。
- 理由: 表示するのは時計・入力ドット・短いメッセージだけ。GPU 文脈を作ると
  ドライバ依存が増える（AMD / Intel の実機で挙動を揃えたい）。
- 却下: `cosmic-text` — シェーピングまで面倒を見るが、今の表示内容には過剰。
- 却下: `tiny-skia` — 角丸矩形のために依存を増やす必要がない。
  矩形とグリフの合成は自前で足りる。
- 補足 (2026-11-15): 角丸矩形と円は SDF のカバレッジで自前実装する
  （ADR-0022）。

## ADR-0008: アイドル検出は当面 swayidle を維持する

- 決定: 自動ロックのトリガは既存の `swayidle` をそのまま使い、`lock` と
  `timeout` の実行先だけ `nekonolock` に差し替える。
- 理由: ロック画面の実装とアイドル検出を同時に変えると、切り分けが難しくなる。
  Sway 1.12 の `ext-idle-notify-v1` 対応を確認できたら、自前タイマーへの
  置き換えを別 phase で検討する。
- 却下: 最初から自前のアイドル管理 — スコープが広がる。

## ADR-0009: 配布は pacman package + ansible role

- 決定: バイナリは [nekono] repo の package `nekonolock`。systemd-user unit と
  PAM ファイルは ansible role。ユーザー設定は dotfiles。
- 理由: ansible-nekonodesk の既存の分担（package / systemd system・user unit は
  ansible、`~/.config` は dotfiles）に合わせる。
- 却下: dotfiles にバイナリを置く — ビルド成果物を git 管理したくない。

## ADR-0010: sudo の YubiKey 認証は今回のスコープ外

- 決定: PIV を外すのに合わせて、sudo はパスワード認証へ戻す。
- 理由: sudo と lock で同じ PAM 経路を共有すると、切り分けが再び難しくなる。
  まずロック画面を安定させる。
- 将来: ヘルパを PAM モジュールから呼ぶ形で追加できる。
- 補足 (2026-11-15): `pam_nekonolock.so` を追加した。ただし
  ユーザーの `nekonolock-auth` socket は呼ばず、署名・検証コードだけを共有する
  （ADR-0027）。

## ADR-0011: モード表示は画面全体で明示する

- 決定: 外周フレーム + 上部 20% の帯 + 入力ドットをモード色で統一し、
  モード名を大きく表示する。切替時は入力中の文字列を破棄する。
- 理由: パスワードを PIN として送るとカードのリトライ回数を消費し、
  3 回でブロックされる。モードの取り違えは「操作ミス」ではなく
  「カードが使えなくなる事故」なので、見落としにくさを優先する。
- 補足: 色だけに頼らず、文字とフレームの太さでも区別する。点滅はしない。
- 補足 (2026-11-15): 実装では `pin` の枠を `frame_width`、`password` の枠を
  `frame_width + 4`（既定 12 px / 16 px）にした。入力ドットは背景に対して
  見えるよう `on_accent`（黄 / 白）で描く。
- 変更 (2026-11-15): 外周フレームと上部 20% の帯は ADR-0022 で廃止した。
  モードは上端の accent line、capsule、入力欄の枠で示す。過去の記録として
  この ADR は残す。

## ADR-0012: Phase 3 まで既定ビルドはキー入力で解除しない

- 決定: `crates/lock` の既定ビルドは、認証が未実装の間、`wl_keyboard` の
  キー入力を受けてもロックを解除しない。Phase 1 の検証に限り
  `insecure-test-unlock` feature を付けたビルドで任意キー解除を有効にする。
- 理由: Phase 1 のプロトタイプをそのままロック画面として起動すると、
  任意のキーで解除される fail-open になる。認証を入れるまで既定を
  fail-closed にし、検証用の逃げ道はビルド時だけに限定する。
- 却下: 環境変数や設定で任意キー解除を有効にする — 実行時に誤って有効化
  でき、配布物にその経路が残る。ビルド時 feature の方が事故を起きにくい。
- 補足: Phase 3 で認証を実装したら、この feature と任意キー解除を削除する。
- 補足 (2026-11-15): Phase 3 で `insecure-test-unlock` feature と分岐を削除した。
  既定ビルドは認証成功以外で `SessionLock::unlock` を呼ばない。

## ADR-0013: PAM は `libpam.so.0` を `dlopen` する

- 決定: `pam-client` を使わず、`libloading` で `libpam.so.0` を実行時に読み、
  `pam_start` / `pam_authenticate` / `pam_acct_mgmt` / `pam_end` を直接呼ぶ。
- 理由: `pam-client` はビルド時に bindgen / clang / libpam のヘッダを要求する。
  CI と配布物のビルドに PAM の開発用パッケージを増やしたくない。実行時の
  libpam は PAM を使う環境には必ずある。
- 却下: `pam-client` — ビルド環境への依存が増える。
- 却下: `pamtester` などの外部コマンド — パスワードを argv / 環境に置く経路が
  生まれやすい。会話関数で直接渡す方が安全。
- 補足: unsafe は `crates/auth/src/pam.rs` の FFI 部分だけに閉じ込める。
  libpam が無い場合は `pam-error` を返し、ロックは維持する。

## ADR-0014: 認証ヘルパは試行回数の状態を持たない

- 決定: ヘルパは PIN / パスワードの失敗回数をプロセス内に保持しない。
  パスワードは `pam_faillock`、PIN はカード側のリトライ回数、UI の連打抑止は
  `[ui] max_attempts` が担う。
- 理由: ヘルパは socket activation で再起動できる。プロセス内のカウンタは
  再起動で消えるため、セキュリティ境界にできない。状態を持つと、解除失敗の
  たびにヘルパを再起動して回避できるので、誤った安心感だけが残る。
- 却下: ヘルパ内の失敗回数で一定回数後に拒否 — 再起動で迂回でき、正規の
  解除手段まで塞ぐ事故になる。
- 補足: UI は失敗理由と最終メッセージを表示する。回数の永続化が必要に
  なったら、PAM / カード側の仕組みを先に検討する。

## ADR-0015: モックバックエンドはテスト専用 feature に閉じる

- 決定: `mock-card` / `mock-pam` はデバッグビルドでのみ有効にし、
  debug assertions の無いビルドでは `compile_error!` にする。
- 理由: モックは既知の PIN / パスワードで解除できる。実行時の環境変数で
  有効化する方式にすると、配布物に解除経路が残る。`insecure-test-unlock` と
  同じくビルド時 feature に限定する。
- 却下: 実行時フラグ — 誤って有効化でき、配布物に経路が残る。


## ADR-0016: OpenPGP の失効・期限切れと鍵指定を厳密に扱う

- 決定: `gpg --verify` の status に `REVKEYSIG` / `KEYREVOKED` / `EXPKEYSIG` /
  `KEYEXPIRED` / `SIGEXPIRED` があれば解除しない。`[auth] key` は 16 桁の
  long key ID か 40 桁の fingerprint（16 進）だけを許可し、照合は key ID なら
  fingerprint の末尾一致、fingerprint なら完全一致にする。
- 理由: OpenPGP カードは鍵の失効を知らないため、失効済みの鍵でも署名できる。
  検証側で status を見ないと、失効したカードでロックを解除できてしまう。
  短い key ID は衝突耐性が低く、部分一致は別の鍵を受け入れる余地を残す。
- 却下: 暗号学的な署名検証だけに任せる — gpg の status を見ないと失効・
  期限切れを区別できない。
- 補足: 16 桁の long key ID は 64 bit なので、より強い識別が必要なら 40 桁の
  fingerprint を設定する。

## ADR-0017: UI の `max_attempts` は恒久ロックではなくクールダウンにする

- 決定: 連続する認証失敗の直後に 1 秒、`[ui] max_attempts` 回に達したら
  5 秒のクールダウンを入れる。`0` で無効。上限に達しても、待てば再試行できる。
- 理由: 目的は Enter の連打や自動入力でヘルパを叩き続けないこと。恒久的に
  拒否すると、正規の解除手段まで塞ぐ事故になる（ADR-0014 と同じ懸念）。
  失敗回数は UI プロセス内だけの状態で、セキュリティ境界にはしない。
  主防御はカード側の PIN リトライと PAM の `pam_faillock`。
- 却下: `max_attempts` 回到達で以後の入力を拒否 — ロック画面を再起動すれば
  回避できるうえ、正規ユーザーを締め出す。
- 却下: クールダウンなし — 連打でヘルパとカードを無駄に叩く。

## ADR-0018: ロック取得を設定・フォントの読み込みより先に行う

- 決定: Wayland に接続して `SessionLock` を取得した後、最大 2 秒フォールバック
  描画でロックサーフェスを出し、その後に設定・フォント・socket パスを読む。
  設定やフォントを読めなくても、既定値とテキストなし描画でロックを維持する。
- 理由: 画面ロッカーが設定エラーで終了すると、画面が覆われない fail-open になる。
  フォントが無い場合でも、外周フレーム・モード帯・入力ドットだけでロック状態を
  維持できる。設定ファイルやフォントが FIFO などでブロックしても、ロックは
  取得済みになる。
- 却下: 起動時に設定・フォントを完全に検証してからロックする — 検証エラーや
  読み込みの遅延がそのまま「ロックされない時間」になる。
- 補足: フォールバック中は文字が出ない。設定とフォントが読めた時点で再描画する。
  設定を読めない間は `CONFIG ERROR - LOCKED` を出し、ログにも理由を残す。

## ADR-0019: systemd user unit は package が持ち、ansible は enable だけする

- 決定: `nekonolock-auth.{socket,service}` は pacman package が
  `/usr/lib/systemd/user/` に配置する。ansible role は unit を
  `/etc/systemd/user/` へ複製せず、`nekonolock-auth.socket` を enable +
  start するだけにする。
- 理由: 同じ unit を package と role の両方に置くと、片方の更新で内容が
  drift する。Arch の慣例では package が `/usr/lib/systemd/user/` に置き、
  管理者は `/etc/systemd/user/` で上書きする。今回はホストごとに変える
  項目が無いので上書きしない。
- 却下: role が `/etc/systemd/user/` に template を置く — package 更新と
  role 更新の二重管理になる。
- 補足: 設計 §12 の初版は package と role の両方に unit を列挙していたが、
  実装で 1 か所に寄せた。role はユーザー manager が動いていない場合に
  socket を有効化せず警告する（プロビジョニング中の失敗を避ける）。

## ADR-0020: PAM は auth と account の両方で login を include する

- 決定: `/etc/pam.d/nekonolock` を `auth include login` と
  `account include login` の 2 行にする。
- 理由: 認証ヘルパは `pam_authenticate` の後に `pam_acct_mgmt` を呼ぶ。
  `account` が無いとアカウントのロックや期限切れを確認できない。
  `login` は `system-login` -> `system-auth` を辿るので、Arch の
  `pam_faillock` が auth / account の両方で効く。
- 却下: `auth include login` だけにする — 設計 §5.2 の記述に近いが、
  `pam_acct_mgmt` の結果を正しく扱えない。
- 補足: package の既定ファイルと ansible role の template は同じ内容に
  する。`packaging/` の検証で両方を確認する。
- 変更 (2026-11-15): auth は `auth include login` をやめ、`system-auth` から
  `pam_pkcs11` を除いた password-only chain にした。account は `login`
  include のまま。理由と却下案は ADR-0031。

## ADR-0021: v0.1.0 tag ができるまで source は commit を pin する

- 決定: Phase 4 の PKGBUILD は `nananek/nekonolock` の Phase 3 コード
  commit の archive を sha256 で pin する。v0.1.0 tag を切ったら tag
  archive に切り替え、sha256 を再計算する。
- 理由: まだ tag / release が無い。pacman repo は `SKIP` checksum を
  認めないため、tag が無い状態でも固定できる commit archive を使う。
  コードは Phase 4 で変わらないので、Phase 3 commit のビルドで目的の
  バイナリになる。
- 却下: `sha256sums=('SKIP')` — supply-chain の固定が消える。
- 補足: 切り替え手順は `packaging/README.md`。commit pin のままでも
  `.SRCINFO` / `.deps.lock` は生成できる。
- 補足 (2026-11-15): v0.1.0 tag を切ったので、`packaging/PKGBUILD` は
  tag archive の pin に切り替えた。

## ADR-0022: UI を Ink 既定に刷新する

- 決定: 既定テーマを濃いチャコールの Ink にする。明るい Paper は同じ UI の
  色違いとして設定例を用意する。外周フレームと上部 20% の帯をやめ、
  上端の accent line、モード capsule、入力欄の枠でモードを示す。
- 理由: 旧 UI はモード帯とフレームが大きく、時計・入力欄との余白が
  不揃いだった。モードの視認性を保ちながら、ロック画面として落ち着いた
  構成にする。Ink は夜間に眩しくない。
- 角丸矩形と円は SDF のカバレッジで自前実装する（ADR-0007 の tiny-skia
  却下を維持）。
- 却下: 外周フレームを残す — 画面端の色面が大きく、モード帯と合わせて
  情報が二重になる。
- 却下: モードを色だけで示す — 取り違えのコストが高い（ADR-0011）。

## ADR-0023: HiDPI は整数倍のバッファで対応する

- 決定: configure の論理サイズに `wl_output.scale` を掛けたバッファを確保し、
  `wl_surface.set_buffer_scale` で論理サイズに戻す。Sway の 1.75 は
  `wl_output.scale = 2` なので 2x で描き、compositor が物理ピクセルへ
  縮小する。
- 理由: smithay-client-toolkit 0.19 は fractional-scale / viewporter の
  ラッパーを持たない。整数倍でも 1x バッファを拡大するより文字と角丸が
  鮮明になる。レイアウトはバッファの比率で置くので倍率に依存しない。
- 却下: `wl_output.scale` を無視して 1x で描く — 現在の 1.75 環境で
  ぼやける（旧実装の問題）。
- 却下: fractional-scale / viewporter を直接 bind する — まず整数倍で
  効果を確認する。必要になったら別 phase で追加する。

## ADR-0024: 無操作時は純黒にして焼き付きを避ける

- 決定: `[ui] blank_after_sec` の無入力で画面全体を純黒にする。キー入力で
  UI を戻し、復帰に使った最初のキーは入力として消費する。認証待ちの間は
  黒くしない。`0` で無効。
- 理由: 有機 EL / 液晶の焼き付きを避ける最小の実装。DPMS やバックライトの
  制御は compositor / ハードウェア依存が大きく、ロック画面の責務を超える。
  純黒でも画面全体の固定表示は避けられる。
- 却下: `swaymsg output * power off` を呼ぶ — Sway 依存で、復帰時の入力と
  サーフェスの扱いが機種依存になる。
- 却下: 時計を数分おきに pixel shift する — 黒画面より効果が小さく、
  実装と検証が増える。

## ADR-0025: テキストは共通ベースラインに載せる

- 決定: 1 行のテキストは、フォントの ascent / descent から行のベースラインを
  求め、すべてのグリフをそこに載せる。行メトリクスを持たないフォントは、
  その行のグリフの上端・下端からベースラインを求める。
- 理由: v0.2.0 はグリフのビットマップをそれぞれ縦中央へ寄せていたため、
  小文字が大文字と同じ高さの帯に浮き、降下部（y, p, g）がベースラインより
  下へ出なかった。ベースラインを共有すれば、大文字・小文字・降下部の
  相対位置はフォントの設計どおりになる。
- 補足: 全大文字化はベースライン修正とは別の表示規約として扱う（ADR-0026）。
- 補足: テストは合成した `Metrics` で、小文字の上端が大文字より下にあり、
  下端が同じベースラインに揃い、降下部だけが下へ出ることを確認する。

## ADR-0026: 画面のコピーはすべて大文字にする

- 決定: 左上ラベル、モード名、案内、状態メッセージ、日付、下端の操作ヒントを
  大文字で出す。時計だけは `clock_format` のとおりに描く。`[ui] label` と
  `[ui] date_format` の結果は描画時に大文字化する。
- 理由: ロック画面の表示を 1 つのタイポグラフィ規則に揃える。v0.2.0 の
  小文字の位置ずれは ADR-0025 のベースライン修正で直したが、大文字と小文字が
  混在するより、短い英語コピーを大文字に統一した方が視線が安定する。
- 補足: 大文字化は表示だけに適用し、設定ファイルの値やログは書き換えない。
  CJK には大文字・小文字の区別がないため、フォントを差し替えた場合も
  そのまま表示される。

## ADR-0027: sudo 用 PAM モジュールはユーザーのヘルパ socket を信頼しない

- 決定: sudo の YubiKey 認証は `pam_nekonolock.so` が行う。署名は実 uid の
  ユーザーとして `gpg` を実行して gpg-agent / scdaemon にさせ、検証は root
  側の一時 keyring に取り込んだ `/etc/nekonolock/sudo-key.asc` だけで行う。
  設定は root 所有の `/etc/nekonolock/sudo.toml` を読み、ユーザーの
  `~/.config/nekonolock/config.toml` と `nekonolock-auth` socket は使わない。
- 理由: `nekonolock-auth` はユーザーが起動・停止・差し替えできる。その応答を
  root の認証に使うと、同じ uid の攻撃者が偽のヘルパを立てて sudo を
  通せてしまう。root への権限境界では、ユーザーが制御できるプロセスの
  応答を信じてはならない。共有するのは `crates/gpg` の署名・検証コードだけに
  する。
- 署名と検証を分ける理由: カードを扱えるのはユーザーの gpg-agent / scdaemon
  だから署名はユーザー側で行う。一方、検証と「どの鍵を信頼するか」は root 側に
  置かないと、ユーザーが自分の鍵で署名して sudo を通せてしまう。
- 失敗時は `PAM_IGNORE` を返し、PAM スタックのパスワード認証へフォールバック
  する。`auth sufficient pam_nekonolock.so` を `auth include system-auth` の
  直前に置く。誤 PIN はカードのリトライ回数だけを消費し、`pam_faillock` には
  数えない（カード側で 3 回ブロックされるため二重に締め出さない）。
- 却下: PAM モジュールからユーザーの `nekonolock-auth` socket に接続する —
  偽のヘルパで sudo が通る。
- 却下: `nekonolock-auth` を system service にして root で動かす — 署名の
  ために結局ユーザーへ `setuid` する必要があり、root 側の検証も別に要る。
  プロセスと socket が増えるだけで、信頼の置き場所は変わらない。
- 却下: ユーザーの `~/.config/nekonolock/config.toml` の `[auth] key` を
  sudo でも使う — ユーザーが鍵を差し替えられる。
- 却下: sudo の YubiKey を既定で必須にする — カードを忘れたときの締め出しを
  避ける。ansible の `nekonolock_sudo_auth` でホストごとに有効化する。
- 補足: PAM モジュールは libpam にリンクせず、実行時に `libpam.so.0` から
  `pam_get_item` を取る（ADR-0013 と同じ方針）。ビルドに PAM の開発用
  パッケージを要求しない。
- 補足: 署名用の一時ディレクトリは root が `/run/user/<uid>` に作り、
  所有者をユーザーへ移す。root が作った 0700 の `/tmp` 配下では、
  `setuid` した gpg がチャレンジを読めないため。
- 補足: 検証用の gpg 子プロセスは、fork 後に実 uid も 0 へ切り替える。
  sudo の PAM は実 uid = 呼び出したユーザー、実効 uid = root で動くが、
  gpg は実 uid でファイルを開くため、root 所有の一時 keyring をそのままでは
  読めない（`Permission denied` になる）。検証は root 所有の入力だけを扱う
  子に限定する。

## ADR-0028: PIN キャッシュを署名前に消す

- 決定: OpenPGP で署名する前に、設定鍵の keygrip を求め、
  `gpg-connect-agent "CLEAR_PASSPHRASE --mode=normal <keygrip>"` で
  gpg-agent の PIN キャッシュを消す。ロック画面と sudo の両方で行う。
- 理由: gpg-agent は一度成功した PIN をキャッシュし、その間は間違った PIN でも
  署名を通す。PIN を第二要素として扱う以上、署名のたびにキャッシュを消す必要が
  ある。
- 却下: `gpg-agent.conf` の `ignore-cache-for-signing` に頼る — ユーザーが
  管理する設定であり、ヘルパや PAM モジュールから強制できない。
- 範囲: この決定が塞ぐのは、`nekonolock` と `pam_nekonolock.so` の署名経路で
  誤 PIN が通る問題だけ。ロック中に同じ uid のプロセスが直接署名する問題は
  止めていない（ADR-0029）。
- 補足: `gpg-connect-agent` はエージェントが `ERR` を返しても終了コード 0 に
  なる。終了コードだけで成功と判断せず、標準出力に `OK` があり `ERR` が無い
  ことを確認できなければ署名しない。

## ADR-0029: 画面ロックをカードアクセスの境界にしない（未対応）

- 状態: 未対応。今回の変更では、ロック中の署名を止めていない。
- 決定: 現時点では、ロック中もユーザーの gpg-agent / scdaemon による署名を
  許す。画面ロックは画面と入力の保護であり、同一 uid のプロセス間の
  権限境界にはしない。
- 理由: ロック時に scdaemon を止めると、SSH / git 署名と競合して旧構成の
  待ちや失敗が戻る。カードアクセスを止めるには、署名経路とは別の仕組みが要る。
- 残るリスク: ロック中にセッションへコード実行の足場を持つ攻撃者は、
  キャッシュが温まっていれば PIN なしで署名できる。sudo の署名も同じ
  ユーザーの gpg-agent に委ねるため、この経路からは止まらない。
- 選べる対応:
  1. ロック開始時とロック中に `CLEAR_PASSPHRASE` を実行する。実装は小さいが、
     署名自体は止まらない。SSH / git は都度 PIN を聞くことになる。
  2. ロック中だけ gpg-agent / scdaemon を止め、解除時に戻す。署名は止まるが、
     SSH / git / agent forwarding も止まる。既定 off の opt-in 向き。
  3. seat / session 単位でカードへのアクセスを落とす（udev、logind、pcscd の
     ポリシーなど）。効果は強いが、マルチシートとリモートを考えると実装と
     検証が大きい。
- 判断: 2 か 3 を既定にするなら、SSH / git をロック中に止めてよいか、
  リモートセッションをどう扱うかを先に決める。決まるまでは現状維持とし、
  画面ロックをカードの権限境界として案内しない。

## ADR-0030: カード抜去の検知はカードがある間 1 秒間隔にする

- 決定: ロック中は、カードがある間は 1 秒ごとに `status` を問い合わせる。
  カードが無いと分かった後は 10 秒間隔に戻す。定期確認の応答は助言として
  扱い、ユーザーの送信と重なったら確認を捨てて送信を優先する。
- 理由: 10 秒間隔だと、カードを抜いてから画面がパスワードモードに変わるまで
  最大 10 秒かかる。その間 PIN 欄に入力すると失敗するため、抜去の反映が
  操作の入口で待たされる。`gpg --card-status` のコストは無視できないが、
  体感の遅れは 1 秒でほぼ消える。
- 却下: 10 秒のまま `Tab` での手動切り替えに任せる — カードを抜いたことに
  気づいていない利用者に操作を増やすだけ。
- 却下: 1 秒より短くする — scdaemon とカードへの負荷が増え、SSH / git の
  署名要求と取り合いになりやすい。
- 補足: 画面が純黒の間は問い合わせない（ADR-0024）。定期確認が失敗しても
  入力中の秘密や表示は変えず、利用者が送信したときに改めてエラーを出す。
## ADR-0031: ロック画面の PAM auth は password-only chain にする

- 決定: `/etc/pam.d/nekonolock` の `auth` は `pam_faillock` + `pam_unix` の
  password-only chain にし、`system-auth` の `pam_pkcs11 sufficient` を
  先に通さない。`account` は `login` include のまま。
- 理由: `auth include login` だと、ロック画面のパスワードが PIV PIN として
  カードに送られ、PIV のリトライを消費する。PIV は SDDM / login / TTY 用に
  残すが、ロック画面の password 経路からは切り離す。
- 却下: `[auth] password = false` — パスワードフォールバックが消える。
- 却下: `system-auth` から `pam_pkcs11` を外す — SDDM の PIV が使えなくなる。
- 補足: `account include login` は `pam_acct_mgmt` のため残す。`pam_faillock`
  の auth / account もそのまま効く。

## ADR-0032: 認証ヘルパの systemd sandbox は使わない

- 決定: `nekonolock-auth.service` は `UMask=0077` だけを残し、capability /
  namespace / filesystem / syscall の制限を入れない。
- 理由: user unit では `CapabilityBoundingSet=` が
  `status=218/CAPABILITIES` で起動できず、`RestrictAddressFamilies=AF_UNIX`
  などは PAM の setuid 経路 (`unix_chkpwd` / `pam_faillock`) を壊して
  ロック画面のパスワードが通らなくなる。認証の可用性を優先する。
- 却下: sandbox を維持して PAM を別プロセスに逃がす — 実装が大きく、
  認証境界が増える。
- 補足: `systemd-analyze security` の exposure は高くなるが、認証境界は
  root 所有の設定・公開鍵と PAM 側で担保する。
