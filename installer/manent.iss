; Manent の Windows インストーラ (Inno Setup 6)
;
; ユーザー単位インストール (%LocalAppData%\Programs\<製品名>)。UAC 昇格は要らない。
; 単一 exe を置くだけ。コード署名はしない (SmartScreen の警告が出る。TrimShot と同じ扱い)。
;
; ビルドは scripts/New-Installer.ps1 から。ISCC へは以下を /D で渡す。
;   MyAppName    : src/main.rs の PRODUCT_NAME (Run キーの値名・mutex 名もこれから作る)
;   MyAppVersion : Cargo.toml の version
;   SourceExe    : リリースビルドの manent.exe
;   OutputDir    : 出力先 (publish)

#ifndef MyAppName
  #error MyAppName is required (pass /DMyAppName=...)
#endif
#ifndef MyAppVersion
  #error MyAppVersion is required (pass /DMyAppVersion=x.y.z)
#endif
#ifndef SourceExe
  #error SourceExe is required (pass /DSourceExe=path\to\manent.exe)
#endif
#ifndef OutputDir
  #define OutputDir "publish"
#endif
#define MyAppExeName "manent.exe"

[Setup]
; AppId はアンインストール項目の同一性を決める。一度決めたら変えない (製品名が変わっても)。
AppId={{06F7CCB1-6FB0-47BC-B68D-DDA7E6D19DFB}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppName}
VersionInfoVersion={#MyAppVersion}

PrivilegesRequired=lowest
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes

; 起動中なら「準備完了」の前に、閉じて続けるか・閉じずに続けるかを選ばせる (Restart Manager)。
; アプリは WM_ENDSESSION で終わる (src/main.rs)。応答しなければ強制終了する。
; AppMutex は使わない。使うと閉じる選択肢が出ず、手で終了するまで先へ進めない。
CloseApplications=force
RestartApplications=no

UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}
; インストーラ自体のアイコン。.iss からの相対 (exe と同じ app-icon.ico を build.rs が埋める)。
SetupIconFile=..\assets\app-icon.ico

Compression=lzma2/max
SolidCompression=yes

OutputDir={#OutputDir}
OutputBaseFilename={#MyAppName}-Setup-{#MyAppVersion}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern

; 起動時に言語を選ばせる。初期値は OS の UI 言語。どちらでもなければ先頭の English になる (Liveback と同じ)。
; 選んだ言語は CurStepChanged でアプリの設定へ渡す。
[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"

[CustomMessages]
english.ScreenClipTask=Use %1 for PrintScreen and Win+Shift+S
japanese.ScreenClipTask=PrintScreen と Win+Shift+S で %1 を使う
english.ScreenClipFinished=Windows Settings will open. Set MS-SCREENCLIP to %1.
japanese.ScreenClipFinished=Windows の設定が開くので、MS-SCREENCLIP を %1 にしてください。
english.RunningUninstall=%1 is running. Close it, then press OK.
japanese.RunningUninstall=%1 が起動中です。終了してから OK を押してください。

[InstallDelete]
; exe 名は 2026-11-08 まで odile.exe だった。AppId が同じなので上書きインストールは旧版のフォルダに入り、旧 exe が残る。
Type: files; Name: "{app}\odile.exe"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"

[Registry]
; ms-screenclip の受け口。アプリも起動のたびに同じものを書く (src/main.rs の write_screenclip_handler)。
; ここでも書くのは、下の [Run] で既定のアプリを開いた時点で Manent を候補に出すため
; (postinstall の起動は nowait なので、アプリの書き込みを待てない)。消すのは CurUninstallStepChanged。
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"
Root: HKCU; Subkey: "Software\{#MyAppName}\Capabilities\URLAssociations"; ValueType: string; ValueName: "ms-screenclip"; ValueData: "{#MyAppName}.ScreenClip"
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "{#MyAppName}"; ValueData: "Software\{#MyAppName}\Capabilities"
Root: HKCU; Subkey: "Software\Classes\{#MyAppName}.ScreenClip"; ValueType: string; ValueName: ""; ValueData: "URL:ms-screenclip"
Root: HKCU; Subkey: "Software\Classes\{#MyAppName}.ScreenClip\Application"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"
Root: HKCU; Subkey: "Software\Classes\{#MyAppName}.ScreenClip\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExeName}"
Root: HKCU; Subkey: "Software\Classes\{#MyAppName}.ScreenClip\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""

[Tasks]
; 既定のアプリはアプリからもインストーラからも書けない (UserChoice はハッシュで守られる)。
; 選ぶと、完了の後に利用者が選ぶページ (Manent の行の一覧) を開く (CurStepChanged)。既に Manent が既定なら出さない。
Name: "screenclip"; Description: "{cm:ScreenClipTask,{#MyAppName}}"; Check: not ScreenClipIsOurs

[Run]
; 初回の案内はアプリが出す (settings.toml が無い起動でトレイのバルーン。src/main.rs の welcome)。
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; Flags: postinstall nowait skipifsilent

[Code]
function AssocQueryString(Flags: Cardinal; Str: Integer; Assoc: String; Extra: Cardinal;
  OutBuf: String; var OutLen: Cardinal): Integer;
  external 'AssocQueryStringW@shlwapi.dll stdcall';

{ ms-screenclip の既定が Manent か。src/main.rs の screenclip_is_ours と同じ読み方
  (UserChoice の ProgId は AppX のまま残るので、AssocQueryString で読む)。 }
function ScreenClipIsOurs(): Boolean;
var
  Buf: String;
  Len: Cardinal;
begin
  Len := 128;
  Buf := StringOfChar(#0, Len);
  { ASSOCF_IS_PROTOCOL = $1000, ASSOCSTR_PROGID = 20 }
  Result := (AssocQueryString($1000, 20, 'ms-screenclip', 0, Buf, Len) = 0) and
    (Copy(Buf, 1, Pos(#0, Buf) - 1) = '{#MyAppName}.ScreenClip');
end;

{ screenclip を選んだときだけ、完了画面の本文で次にすることを伝える。 }
procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpFinished) and WizardIsTaskSelected('screenclip') then
    WizardForm.FinishedLabel.Caption := WizardForm.FinishedLabel.Caption + #13#10#13#10 +
      FmtMessage(CustomMessage('ScreenClipFinished'), ['{#MyAppName}']);
end;

{ 完了を押した後に、既定のアプリの Manent のページを開く。
  インストールの前に、選んだ言語をアプリへ渡す (HKCU\Software\<製品名>\Language)。
  OS の UI 言語と違うときだけ書く。同じなら設定は「自動」のままでよいので、前回の残りを消す。
  アプリは次の起動でこの値を設定の言語へ移して消す (src/main.rs の take_installer_language)。 }
procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
  OsJa: Boolean;
begin
  if CurStep = ssInstall then
  begin
    OsJa := (GetUILanguage and $3FF) = $11;
    RegDeleteValue(HKCU, 'Software\{#MyAppName}', 'Language');
    if (ActiveLanguage = 'japanese') and not OsJa then
      RegWriteStringValue(HKCU, 'Software\{#MyAppName}', 'Language', 'ja')
    else if (ActiveLanguage = 'english') and OsJa then
      RegWriteStringValue(HKCU, 'Software\{#MyAppName}', 'Language', 'en');
  end;
  if (CurStep = ssDone) and WizardIsTaskSelected('screenclip') and not WizardSilent then
    ShellExec('open', 'ms-settings:defaultapps?registeredAppUser={#MyAppName}', '', '',
      SW_SHOWNORMAL, ewNoWait, Code);
end;

{ アンインストーラは Restart Manager を使わない。起動中なら従来どおり終了を促す。 }
function InitializeUninstall(): Boolean;
begin
  Result := True;
  while Result and CheckForMutexes('Local\{#MyAppName}-single-instance') do
    Result := MsgBox(FmtMessage(CustomMessage('RunningUninstall'), ['{#MyAppName}']),
      mbError, MB_OKCANCEL) = IDOK;
end;

{ 自動起動はアプリ自身が HKCU の Run キーで管理する。インストーラは登録しない。
  アンインストール時だけ、残った値を消す (消さないと存在しない exe を毎回起動しようとする)。
  APPDATA 配下の設定フォルダは消さずに残す。 }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#MyAppName}');
    { エクスプローラの画像の右クリックのメニューも、アプリが起動のたびに書く。
      消さないと、存在しない exe を指す項目が残る。 }
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Classes\SystemFileAssociations\image\shell\{#MyAppName}');
    { ms-screenclip の受け口 (既定のアプリの候補)。アプリが起動のたびに書く。 }
    RegDeleteValue(HKCU, 'Software\RegisteredApplications', '{#MyAppName}');
    { インストーラの言語の選択。アプリを一度も起動しないと残る。 }
    RegDeleteValue(HKCU, 'Software\{#MyAppName}', 'Language');
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\{#MyAppName}\Capabilities');
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Classes\{#MyAppName}.ScreenClip');
  end;
end;
