; Inno Setup script for the osu! Map Manager Windows installer.
;
; Compiled by .github/workflows/release.yml, which stamps the release tag's
; version via /DAppVersion; a plain `iscc` run falls back to 0.0.0. The
; packaged exe is read straight from `target/release` (build it with
; `cargo build --release` first).

#ifndef AppVersion
#define AppVersion "0.0.0"
#endif

[Setup]
; Stable identity so upgrades replace the previous install instead of
; stacking a second one.
AppId={{39302626-0751-446A-B69C-61480EF09C17}}
AppName=osu! Map Manager
AppVersion={#AppVersion}
VersionInfoVersion={#AppVersion}
AppPublisher=Stanislav
AppPublisherURL=https://github.com/feelvm/osu-map-manager
AppSupportURL=https://github.com/feelvm/osu-map-manager/issues
AppCopyright=Copyright (C) 2026 Stanislav
; Licensed GPL-3.0-only; see LICENSE at the repository root.
; Per-user install by default (no UAC prompt), with an "install for all
; users" choice offered in the wizard and via /ALLUSERS on the command line.
PrivilegesRequiredOverridesAllowed=dialog commandline
DefaultDirName={autopf}\osu! Map Manager
DefaultGroupName=osu! Map Manager
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\osu-map-manager.exe
; Setup wizard, Add/Remove Programs and shortcut icons all come from the
; app's own icon, the same app.ico embedded in the exe.
SetupIconFile=..\..\app.ico
OutputBaseFilename=osu-map-manager-windows-x86_64-setup
; The published exe is x86_64-only; install it into the real Program Files
; instead of Program Files (x86) and refuse 32-bit Windows.
ArchitecturesInstallIn64BitMode=x64compatible
ArchitecturesAllowed=x64compatible
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes

[Files]
Source: "..\..\target\release\osu-map-manager.exe"; DestDir: "{app}"; Flags: ignoreversion

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Icons]
Name: "{group}\osu! Map Manager"; Filename: "{app}\osu-map-manager.exe"
Name: "{autodesktop}\osu! Map Manager"; Filename: "{app}\osu-map-manager.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\osu-map-manager.exe"; Description: "{cm:LaunchProgram,osu! Map Manager}"; Flags: nowait postinstall skipifsilent
