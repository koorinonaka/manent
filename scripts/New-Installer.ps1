<#
.SYNOPSIS
  リリースビルドし、publish\ にインストーラ (Inno Setup) とポータブル zip を作る。
  CI (.github/workflows/release.yml) も同じスクリプトを呼ぶ。
.PARAMETER SkipBuild
  cargo build を省き、既存の target\release\manent.exe から作る。
#>
param([switch]$SkipBuild)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$iss = Join-Path $repoRoot "installer\manent.iss"
$exe = Join-Path $repoRoot "target\release\manent.exe"
$outDir = Join-Path $repoRoot "publish"

# バージョンの単一の情報源は Cargo.toml、製品名は src/main.rs の PRODUCT_NAME。
$version = (Select-String -Path (Join-Path $repoRoot "Cargo.toml") -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$name = (Select-String -Path (Join-Path $repoRoot "src\main.rs") -Pattern 'pub const PRODUCT_NAME: &str = "(.+)";' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version -or -not $name) { throw "version ($version) か PRODUCT_NAME ($name) が読めません" }
Write-Host "製品名: $name / バージョン: $version"

# ISCC.exe はユーザー単位・システム全体のどちらにも入りうる。
$iscc = @(
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
    (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
    (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe")
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { $iscc = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source }
if (-not $iscc) { throw "ISCC.exe が見つかりません。``winget install JRSoftware.InnoSetup`` で導入してください。" }
Write-Host "ISCC: $iscc"

if (-not $SkipBuild) {
    Push-Location $repoRoot
    try {
        & cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw "cargo build に失敗しました" }
    } finally { Pop-Location }
}
if (-not (Test-Path $exe)) { throw "exe がありません: $exe" }

New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$zip = Join-Path $outDir "$name-$version-portable.zip"
Compress-Archive -Path $exe -DestinationPath $zip -Force

& $iscc "/DMyAppName=$name" "/DMyAppVersion=$version" "/DSourceExe=$exe" "/DOutputDir=$outDir" $iss
if ($LASTEXITCODE -ne 0) { throw "ISCC に失敗しました" }
$setup = Join-Path $outDir "$name-Setup-$version.exe"
if (-not (Test-Path $setup)) { throw "インストーラが生成されませんでした: $setup" }

Get-Item $setup, $zip | ForEach-Object { Write-Host ("{0}  {1:N0} bytes" -f $_.Name, $_.Length) }
