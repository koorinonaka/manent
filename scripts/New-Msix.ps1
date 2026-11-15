<#
.SYNOPSIS
  ストア版の MSIX を作り、publish\ に .msixupload (.msix を入れた zip) を置く。
  CI (.github/workflows/release.yml) も同じスクリプトを呼ぶ。GitHub 版 (New-Installer.ps1) とは別物。
.PARAMETER Identity
  auto (既定): packaging\identity.toml の [store] に値があればそれ、空なら [local] で作って警告する。
  store: [store] で作る。空なら止まる。  local: [local] で作る (手元の検証用。ストアには出せない)。
  ファイル名は store が Manent-<ver>.msixupload、local が Manent-<ver>-local.msixupload。
.PARAMETER SkipBuild
  cargo build を省き、既存の target\release\manent.exe と manent_shell.dll から作る。
.NOTES
  レイアウト target\msix\layout\ は残す。手元では Add-AppxPackage -Register target\msix\layout\AppxManifest.xml
  で登録できる (開発者モード)。
#>
param(
    [ValidateSet('auto', 'store', 'local')] [string]$Identity = 'auto',
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$packaging = Join-Path $repoRoot "packaging"
$release = Join-Path $repoRoot "target\release"
$msixDir = Join-Path $repoRoot "target\msix"
$layout = Join-Path $msixDir "layout"
$outDir = Join-Path $repoRoot "publish"

# Version: Cargo.toml の version に .0 を足した 4 桁。ストアは 4 桁目が 0 であることを求める。
$version = (Select-String -Path (Join-Path $repoRoot "Cargo.toml") -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw "Cargo.toml の version が読めません: '$version'" }

# CLSID の単一の情報源は DLL の定数。
$hex = (Select-String -Path (Join-Path $repoRoot "shellext\src\lib.rs") -Pattern 'GUID::from_u128\(0x([0-9a-fA-F_]+)\)' |
    Select-Object -First 1).Matches[0].Groups[1].Value -replace '_', ''
if ($hex.Length -ne 32) { throw "shellext\src\lib.rs の CLSID が読めません" }
$clsid = '{0}-{1}-{2}-{3}-{4}' -f $hex.Substring(0, 8), $hex.Substring(8, 4), $hex.Substring(12, 4), $hex.Substring(16, 4), $hex.Substring(20)

# identity.toml は [section] と key = "value" だけを読む。
$ids = @{}
$section = $null
foreach ($line in Get-Content -Encoding UTF8 (Join-Path $packaging "identity.toml")) {
    if ($line -match '^\s*\[(\w+)\]\s*$') { $section = $Matches[1]; $ids[$section] = @{}; continue }
    if ($section -and $line -match '^\s*(\w+)\s*=\s*"(.*)"\s*$') { $ids[$section][$Matches[1]] = $Matches[2] }
}
function Test-Complete($s) { $s -and $s.name -and $s.publisher -and $s.publisher_display_name }
if ($Identity -eq 'auto') {
    if (Test-Complete $ids['store']) { $Identity = 'store' } else {
        Write-Warning "packaging\identity.toml の [store] が空なので [local] で作ります。この .msixupload はストアに出せません。"
        $Identity = 'local'
    }
}
$id = $ids[$Identity]
if (-not (Test-Complete $id)) { throw "packaging\identity.toml の [$Identity] に name・publisher・publisher_display_name がそろっていません" }
$suffix = if ($Identity -eq 'local') { '-local' } else { '' }
$base = "Manent-$version$suffix"
Write-Host "Identity: [$Identity] $($id.name) / $($id.publisher) / Version $version.0 / CLSID $clsid"

# makeappx.exe: Windows SDK の最も新しい版。
$kits = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
$makeappx = Get-ChildItem -Path $kits -Directory -Filter "10.*" -ErrorAction SilentlyContinue |
    Sort-Object { [version]$_.Name } -Descending |
    ForEach-Object { Join-Path $_.FullName "x64\makeappx.exe" } |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $makeappx) { throw "makeappx.exe が見つかりません ($kits\10.*\x64)。Windows SDK を導入してください。" }
Write-Host "makeappx: $makeappx"
# makepri.exe は makeappx.exe と同じ場所にある。
$makepri = Join-Path (Split-Path $makeappx) "makepri.exe"
if (-not (Test-Path $makepri)) { throw "makepri.exe が見つかりません ($makepri)" }

if (-not $SkipBuild) {
    Push-Location $repoRoot
    try {
        # default-members に shellext が入っているので、exe と DLL が一度に作られる。
        & cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw "cargo build に失敗しました" }
    } finally { Pop-Location }
}
$exe = Join-Path $release "manent.exe"
$dll = Join-Path $release "manent_shell.dll"
foreach ($f in $exe, $dll) { if (-not (Test-Path $f)) { throw "ビルドの成果物がありません: $f" } }

if (Test-Path $layout) { Remove-Item -Recurse -Force $layout }
New-Item -ItemType Directory -Force -Path (Join-Path $layout "Assets"), $outDir | Out-Null
Copy-Item $exe, $dll $layout
Copy-Item (Join-Path $packaging "Assets\*.png") (Join-Path $layout "Assets")
$manifest = (Get-Content -Raw -Encoding UTF8 (Join-Path $packaging "AppxManifest.xml")).
    Replace('{{NAME}}', $id.name).
    Replace('{{PUBLISHER}}', $id.publisher).
    Replace('{{PUBLISHER_DISPLAY_NAME}}', $id.publisher_display_name).
    Replace('{{VERSION}}', "$version.0").
    Replace('{{CLSID}}', $clsid)
if ($manifest -match '\{\{\w+\}\}') { throw "マニフェストに埋めていない印が残っています: $($Matches[0])" }
[IO.File]::WriteAllText((Join-Path $layout "AppxManifest.xml"), $manifest, (New-Object Text.UTF8Encoding $false))

# resources.pri: これが無いと Square44x44Logo.targetsize-*_altform-unplated.png が引かれず、
# タスクバーとスタートのアイコンがアクセント色の板に載る。
$priconfig = Join-Path $msixDir "priconfig.xml"
& $makepri createconfig /cf $priconfig /dq en-US /pv 10.0.0 /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makepri createconfig に失敗しました" }
& $makepri new /pr $layout /cf $priconfig /mn (Join-Path $layout "AppxManifest.xml") /of (Join-Path $layout "resources.pri") /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makepri new に失敗しました" }

$msix = Join-Path $msixDir "$base.msix"
& $makeappx pack /o /d $layout /p $msix
if ($LASTEXITCODE -ne 0) { throw "makeappx pack に失敗しました" }

# .msixupload は .msix を入れた zip。Compress-Archive は .zip の名前しか受けないので、作ってから改名する。
$zip = Join-Path $msixDir "$base.zip"
$upload = Join-Path $outDir "$base.msixupload"
Remove-Item $zip, $upload -ErrorAction SilentlyContinue
Compress-Archive -Path $msix -DestinationPath $zip
Move-Item $zip $upload
Get-Item $upload | ForEach-Object { Write-Host ("{0}  {1:N0} bytes" -f $_.Name, $_.Length) }
