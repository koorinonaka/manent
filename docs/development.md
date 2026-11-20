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

版を上げるときは、タグを打つ前にマニュアル (`site/manual.html`) も合わせます。

1. `Cargo.toml` の `version` を上げます。
2. `publish\release-notes-<版>.md` にリリースノートを書きます。
3. リリースノートの項目を 1 つずつマニュアルの節と照らします。
4. 違う節があれば、文言を直します。図が変わる節は、図を撮り直します。
5. マニュアルのフッターの対応版 (`.m-foot-v`、日本語と英語の 2 か所) を新しい版にします。
   マニュアルの文言とフッターの版は `.agents/tasks/evidence/t261114-e473-build-the-manual-page/copy.md` が正です。
   直したら、同じフォルダの `build.js` を `node` で実行して `site/manual.html` を書き直します。
6. commit してから、続けてタグを打ちます。

`site/` は master に push した時点で公開されます。そのため、マニュアルは機能ごとに直さず、リリースの直前にまとめて直します。
フッターの対応版が `Cargo.toml` の版と違うと、タグの CI はビルドの前に失敗します。

## ライセンス

Manent は [MIT](../LICENSE) です。依存する crate (`gif`、`lz4_flex`、`quantette`、`serde`、
`toml`、`windows`、`windows-numerics`) は、すべて MIT または Apache-2.0 で使えます。

GIF の色の量子化には `quantette` を使います。よく使われる gifski (AGPL) と
imagequant (GPL) は、MIT で配る Manent とライセンスが合わないため使いません。
