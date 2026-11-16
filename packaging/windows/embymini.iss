; Windows installer (Inno Setup 6.5+; ChineseSimplified.isl is the 6.x translation, Inno 7 ships its own). Built in CI:
;   iscc /DAppVersion=<version> packaging\windows\embymini.iss
; Per-user install, no admin rights. All app data lives in data\ next to the exe (created by the app);
; uninstalling asks whether to delete it.
; /UPDATE=1 (passed by the in-app updater together with /VERYSILENT) starts the app again when setup finishes.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=x.y.z
#endif

[Setup]
AppId={{8510A754-B556-45EE-A295-903A85E4EEA0}
AppName=EmbyMini
AppVersion={#AppVersion}
AppVerName=EmbyMini {#AppVersion}
AppPublisher=stqfdyr
DefaultDirName={localappdata}\Programs\EmbyMini
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\target\installer
OutputBaseFilename=EmbyMini_{#AppVersion}_x64-setup
SetupIconFile=..\..\src-tauri\icons\icon.ico
UninstallDisplayIcon={app}\embymini.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Close a running copy before replacing files (the updater has already shut it down; this covers manual installs)
CloseApplications=force
RestartApplications=no

[Languages]
Name: "chs"; MessagesFile: "ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\..\target\release\embymini.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\src-tauri\mpv\*"; DestDir: "{app}\mpv"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\EmbyMini"; Filename: "{app}\embymini.exe"
Name: "{autodesktop}\EmbyMini"; Filename: "{app}\embymini.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\embymini.exe"; Description: "{cm:LaunchProgram,EmbyMini}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\embymini.exe"; Flags: nowait; Check: IsUpdate

[Code]
function IsUpdate: Boolean;
begin
  Result := ExpandConstant('{param:UPDATE|0}') = '1';
end;

// data\ holds servers, logins and caches. Delete it only if it is ours (the app leaves a .embymini marker)
// and the user agrees; a silent uninstall keeps it.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Data: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    Data := ExpandConstant('{app}\data');
    if FileExists(Data + '\.embymini') and not UninstallSilent then
      if MsgBox('是否同时删除服务器配置和缓存（' + Data + '）？', mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      begin
        DelTree(Data, True, True, True);
        RemoveDir(ExpandConstant('{app}'));
      end;
  end;
end;
