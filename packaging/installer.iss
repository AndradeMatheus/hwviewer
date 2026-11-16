; HWiNFO Viewer installer (Inno Setup 6). Built by packaging\build.ps1, which passes /DAppVersion=x.y.z.
; Installs to Program Files, registers the HWiNFO watchdog (an elevated logon task) and optional autostart.
; User data lives in %LOCALAPPDATA%\HWiNFO Viewer and survives upgrades.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define AppName "HWiNFO Viewer"
#define AppExe "hwviewer.exe"
#define TaskName "HWiNFO Viewer Watchdog"

[Setup]
; never change AppId: upgrades and uninstall are matched by it
AppId={{A5D40660-4269-4252-8451-AC82C7B19426}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=AndradeMatheus
AppPublisherURL=https://github.com/AndradeMatheus/hwviewer
AppSupportURL=https://github.com/AndradeMatheus/hwviewer/issues
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=HWiNFO-Viewer-Setup-{#AppVersion}
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; the running app/watchdog hold the exe open: let Restart Manager close them
CloseApplications=yes
RestartApplications=no
; HKCU autostart is written for the user running setup (normally the same account, elevated)
UsedUserAreasWarning=no

[Tasks]
Name: "watchdog"; Description: "Install the HWiNFO watchdog (recommended for the free HWiNFO: restarts it silently before the 12-hour shared-memory limit)"; GroupDescription: "HWiNFO:"
Name: "autostart"; Description: "Start {#AppName} with Windows"; GroupDescription: "Startup:"
Name: "autostart\tray"; Description: "Tray only at startup (no window, lightest)"; GroupDescription: "Startup:"; Flags: unchecked
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\dist\HWViewer\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
; FPS fallback for the in-game OSD (run by the elevated watchdog when RTSS isn't running). MIT, Intel.
Source: "..\dist\HWViewer\tools\PresentMon.exe"; DestDir: "{app}\tools"; Flags: ignoreversion
Source: "..\dist\HWViewer\tools\PresentMon-LICENSE.txt"; DestDir: "{app}\tools"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autoprograms}\{#AppName} (tray only)"; Filename: "{app}\{#AppExe}"; Parameters: "tray"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Tasks: autostart and not autostart\tray; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"" tray"; Tasks: autostart\tray; Flags: uninsdeletevalue

[Run]
; replaces any existing watchdog task (including the old Python one) and starts it
Filename: "{app}\{#AppExe}"; Parameters: "watchdog install"; StatusMsg: "Installing the HWiNFO watchdog..."; Flags: runhidden waituntilterminated; Tasks: watchdog
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{app}\{#AppExe}"; Parameters: "watchdog uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveWatchdog"

[Code]
// Stop the watchdog task before files are replaced (it runs our exe elevated, outside Restart Manager's reach).
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\schtasks.exe'), '/End /TN "{#TaskName}"', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

// When the watchdog task isn't wanted, remove any previous one so it doesn't run a stale exe.
procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
begin
  if (CurStep = ssPostInstall) and not WizardIsTaskSelected('watchdog') then
    Exec(ExpandConstant('{app}\{#AppExe}'), 'watchdog uninstall', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
    if MsgBox('Also delete your settings, profiles and logs (' + ExpandConstant('{localappdata}') + '\{#AppName})?',
              mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      DelTree(ExpandConstant('{localappdata}\{#AppName}'), True, True, True);
end;
