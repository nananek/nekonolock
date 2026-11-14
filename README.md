# nekonolock

Sway (wlroots) 用の小さなスクリーンロッカー。YubiKey の OpenPGP applet と
パスワードの両方で解除でき、scdaemon と pcscd のカード取り合いを
構造的に起こさないことを目的とする。

**状態: Phase 2 完了（プロトコルと認証ヘルパ）**

`crates/proto` に要求・応答の型、socket、設定を、`crates/auth` に OpenPGP の
チャレンジ署名と PAM 認証を実装した。UI 側の認証連携は Phase 3。

```sh
# 単体テストと結合テスト（モックバックエンド）
cargo test --workspace
cargo test -p nekonolock-auth --features mock-card,mock-pam

# 手元でヘルパを起動する場合
cargo run -p nekonolock-auth -- --config ~/.config/nekonolock/config.toml
```

ロック画面の既定ビルドは、Phase 3 で認証を繋ぐまでキー入力を受けても解除しない。
Phase 1 の検証には `cargo run -p nekonolock --features insecure-test-unlock` を使う。

- 設計: [docs/design.md](docs/design.md)
- 決定と却下案: [docs/decisions.md](docs/decisions.md)
