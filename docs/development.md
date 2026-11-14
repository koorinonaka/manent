# 開発

Manent をソースからビルドする手順です。Windows 11 (x64) で動かします。

## 必要なもの

- Rust。版は `rust-toolchain.toml` で固定しています (1.97.1、`x86_64-pc-windows-msvc`)。
  `rustup` を入れておけば、初回のビルドで自動的に入ります。
- Visual Studio の C++ ビルドツール (MSVC のリンカ)。
- インストーラを作るときだけ [Inno Setup 6](https://jrsoftware.org/isinfo.php)。
  `winget install JRSoftware.InnoSetup` で入ります。

## ビルドと実行

```powershell
cargo build --release
.\target\release\manent.exe
```

実行ファイルは単体で動きます。起動するとトレイに常駐します。

## テスト

```powershell
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## インストーラとポータブル zip

```powershell
pwsh scripts\New-Installer.ps1
```

リリースビルドをしてから、`publish\` に次の 2 つを作ります。

- `Manent-Setup-<版>.exe`: Inno Setup のインストーラ
- `Manent-<版>-portable.zip`: 単体の exe を入れた zip

ビルド済みの `target\release\manent.exe` から作るときは `-SkipBuild` を付けます。

## リリース

版の正は `Cargo.toml` の `version` の 1 箇所です。インストーラの版とファイル名は、
`scripts\New-Installer.ps1` がここから読みます。

リリースは `v<版>` のタグ (例: `v0.9.0`) ごとに作ります。タグを打つと CI が
インストーラとポータブル zip を作り、このリポジトリの Releases に添付します。

## ライセンス

Manent は [MIT](../LICENSE) です。依存する crate (`gif`、`lz4_flex`、`quantette`、`serde`、
`toml`、`windows`、`windows-numerics`) は、すべて MIT または Apache-2.0 で使えます。

GIF の色の量子化には `quantette` を使います。よく使われる gifski (AGPL) と
imagequant (GPL) は、MIT で配る Manent とライセンスが合わないため使いません。
