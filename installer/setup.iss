; Screenshot Sorter installer (Inno Setup 6).
; Built by release.ps1, which passes /DAppVersion=x.y.z
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppName "Screenshot Sorter"
#define AppExe "Screenshot Sorter.exe"
#define RepoUrl "https://github.com/JustSad42/screenshot-sorter"

[Setup]
AppId={{6E0B7C51-3F2A-4B8E-9D1C-5A7F2E9B4C30}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=JustSad42
AppPublisherURL={#RepoUrl}
AppSupportURL={#RepoUrl}/issues
AppUpdatesURL={#RepoUrl}/releases
; per-user install: no admin prompt, and the app can keep its data next to the .exe
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\{#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UsePreviousAppDir=yes
OutputDir=..\dist
OutputBaseFilename=ScreenshotSorter-{#AppVersion}-setup
SetupIconFile=..\rust\icons\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\rust\target\release\screenshot-sorter.exe"; DestDir: "{app}"; DestName: "{#AppExe}"; Flags: ignoreversion
Source: "How to use.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
; register the Explorer menu / login entry for this copy, and restart background sorting if it's on
Filename: "{app}\{#AppExe}"; Parameters: "--setup"; Flags: runhidden
Filename: "{app}\{#AppExe}"; Description: "Open {#AppName}"; Flags: nowait postinstall skipifsilent
; after an in-app update (silent install), open the app again
Filename: "{app}\{#AppExe}"; Flags: nowait skipifnotsilent

[UninstallRun]
Filename: "{app}\{#AppExe}"; Parameters: "--remove-integration"; Flags: runhidden; RunOnceId: "RemoveIntegration"

[UninstallDelete]
; caches only - settings, categories and any "originals" backup are left in place
Type: filesandordirs; Name: "{app}\thumbs"
Type: files; Name: "{app}\update_state.json"
Type: files; Name: "{app}\library.json"

[Code]
// Stop background sorting before files are replaced.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
  Exe: String;
begin
  Exe := ExpandConstant('{app}\{#AppExe}');
  if FileExists(Exe) then
    Exec(Exe, '--stop', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;
