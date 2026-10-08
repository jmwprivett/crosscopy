; Inno Setup script for CrossCopy. Built by the release workflow:
;   iscc /DAppVersion=0.2.0 packaging\windows\crosscopy.iss
; Expects release binaries in target\release and dist\crosscopy.ico
; (`cargo xtask windows-ico dist/crosscopy.ico`).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{8F3C2A51-6D4B-4E9A-9C1F-2B7E5D0A4C11}
AppName=CrossCopy
AppVersion={#AppVersion}
AppPublisher=jmwprivett
AppPublisherURL=https://github.com/jmwprivett/crosscopy
; Per-user install: no admin prompt, and the app can update itself.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\CrossCopy
DisableDirPage=yes
DisableProgramGroupPage=yes
OutputDir=..\..\dist
OutputBaseFilename=CrossCopy-{#AppVersion}-windows-x86_64-setup
SetupIconFile=..\..\dist\crosscopy.ico
UninstallDisplayIcon={app}\CrossCopy.exe
UninstallDisplayName=CrossCopy
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Close a running CrossCopy before replacing its files.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "autostart"; Description: "Start CrossCopy when I sign in"

[Files]
Source: "..\..\target\release\crosscopy-tray.exe"; DestDir: "{app}"; DestName: "CrossCopy.exe"; Flags: ignoreversion
Source: "..\..\target\release\crosscopy.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\CrossCopy"; Filename: "{app}\CrossCopy.exe"

[Registry]
; Same value the app's "Start at login" toggle manages. Not touched during
; self-updates, so a choice made in the app sticks.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "CrossCopy"; ValueData: """{app}\CrossCopy.exe"""; Flags: uninsdeletevalue; Tasks: autostart; Check: not IsUpdate

[Run]
Filename: "{app}\CrossCopy.exe"; Description: "Start CrossCopy"; Flags: nowait postinstall skipifsilent
; Self-update (`/update=1`): restart the app after a silent install.
Filename: "{app}\CrossCopy.exe"; Flags: nowait; Check: IsUpdate

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/IM CrossCopy.exe /F"; Flags: runhidden; RunOnceId: "StopCrossCopy"

[Code]
function IsUpdate: Boolean;
begin
  Result := ExpandConstant('{param:update|0}') = '1';
end;
