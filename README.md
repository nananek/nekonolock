# nekonolock

Sway (wlroots) 用の小さなスクリーンロッカー。YubiKey の OpenPGP applet と
パスワードの両方で解除でき、scdaemon と pcscd のカード取り合いを
構造的に起こさないことを目的とする。

**状態: Phase 3 完了（ロック画面の入力・描画・認証連携）**

`crates/lock` に `ext-session-lock-v1` の全出力ロック、モード帯・外周フレーム・
時計・入力ドットの描画、`nekonolock-auth` への認証要求を実装した。認証が
成功するまでロックは解除しない。

```sh
# 単体テストと結合テスト（モックバックエンド）
cargo test --workspace
cargo test -p nekonolock-auth --features mock-card,mock-pam

# 手元でヘルパを起動する場合
cargo run -p nekonolock-auth -- --config ~/.config/nekonolock/config.toml

# ロック画面（別端末から Sway 上で）
cargo run -p nekonolock

# Sway headless の E2E（Docker + モック認証ヘルパ）
bash spike/03-lock-ui-sway.sh
```

ビルドには `libxkbcommon` の開発用ファイル（Arch: `libxkbcommon`、
Debian/Ubuntu: `libxkbcommon-dev`）が要る。実行時は `[ui] font` のフォントと
`nekonolock-auth` の socket activation を用意する。詳細は設計の Phase 3 を参照。

- 設計: [docs/design.md](docs/design.md)
- 決定と却下案: [docs/decisions.md](docs/decisions.md)
