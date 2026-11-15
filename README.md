# nekonolock

Sway (wlroots) 用の小さなスクリーンロッカー。YubiKey の OpenPGP applet と
パスワードの両方で解除でき、scdaemon と pcscd のカード取り合いを
構造的に起こさないことを目的とする。

**状態: Phase 4 完了（pacman package と ansible role）**

`packaging/` に pacman package と ansible role のステージングを置いた。
package は `nekonolock` / `nekonolock-auth`、`/etc/pam.d/nekonolock`、
`SOCK_SEQPACKET` の systemd user unit 2 本を含む。ansible role は package
導入、PAM 配置、socket の enable、swayidle の差し替えを行う。

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

# package と ansible role の E2E（Arch Linux コンテナ）
bash spike/04-package-ansible.sh
```

ビルドには `libxkbcommon` の開発用ファイル（Arch: `libxkbcommon`、
Debian/Ubuntu: `libxkbcommon-dev`）が要る。実行時は `[ui] font` のフォントと
`nekonolock-auth` の socket activation を用意する。pacman package と
ansible role の内容は [packaging/README.md](packaging/README.md) を参照。

- 設計: [docs/design.md](docs/design.md)
- 決定と却下案: [docs/decisions.md](docs/decisions.md)
- 配布物: [packaging/README.md](packaging/README.md)
