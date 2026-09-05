#ifndef AppVersion
  #error AppVersion must be supplied by package-installer.ps1
#endif

[Setup]
AppId={{AC4F1763-542E-4A8F-B635-C9B6C685FC39}
AppName=Codex Taskbar
AppVersion={#AppVersion}
AppPublisher=Codex Taskbar
DefaultDirName={localappdata}\Programs\CodexTaskbar
DefaultGroupName=Codex Taskbar
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=codex-taskbar-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\codex-taskbar.exe
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\target\release\codex-taskbar.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Codex Taskbar"; Filename: "{app}\codex-taskbar.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\Codex Taskbar"; Filename: "{app}\codex-taskbar.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\codex-taskbar.exe"; Description: "Launch Codex Taskbar"; Flags: nowait postinstall skipifsilent

; Deliberately no UninstallDelete entry: retain the user's SQLite history/settings.
