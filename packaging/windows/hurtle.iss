; Inno Setup script for Hurtle (per-user, no admin rights).
;   iscc /DVersion=0.2.2 /DSrcDir=C:\path\to\dir-with-the-three-exes /DOutDir=dist packaging\windows\hurtle.iss
; Output: <OutDir>\hurtle-<Version>-windows-x86_64-setup.exe
#ifndef Version
  #error Pass /DVersion=x.y.z
#endif
#ifndef SrcDir
  #error Pass /DSrcDir=<dir with hurtle.exe, hurtle-host.exe, hurtle-cli.exe>
#endif
#ifndef OutDir
  #define OutDir "."
#endif

[Setup]
AppId={{ADB9B7BE-7CE8-4493-8F58-025AF5147BDB}
AppName=Hurtle
AppVersion={#Version}
AppPublisher=arafays
AppPublisherURL=https://github.com/arafays/hurtle
DefaultDirName={localappdata}\Programs\Hurtle
DefaultGroupName=Hurtle
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutDir}
OutputBaseFilename=hurtle-{#Version}-windows-x86_64-setup
SetupIconFile=hurtle.ico
UninstallDisplayIcon={app}\hurtle.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
CloseApplications=yes

[Tasks]
Name: desktopicon; Description: "Create a &desktop icon"; Flags: unchecked
Name: autostart; Description: "Start Hurtle when I sign in"; Flags: unchecked
Name: addtopath; Description: "Add hurtle-cli to PATH"

[Files]
Source: "{#SrcDir}\hurtle.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SrcDir}\hurtle-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SrcDir}\hurtle-cli.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Hurtle"; Filename: "{app}\hurtle.exe"
Name: "{autodesktop}\Hurtle"; Filename: "{app}\hurtle.exe"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Hurtle"; ValueData: """{app}\hurtle.exe"""; Flags: uninsdeletevalue; Tasks: autostart
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{code:AppendPath}"; Check: NeedsPath; Tasks: addtopath

[Run]
Filename: "{app}\hurtle-host.exe"; Parameters: "--register"; Flags: runhidden; StatusMsg: "Registering with browsers..."
Filename: "{app}\hurtle.exe"; Description: "Start Hurtle"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\hurtle-host.exe"; Parameters: "--unregister"; Flags: runhidden; RunOnceId: "UnregisterHost"

[Code]
function AppendPath(Param: string): string;
var
  P: string;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', P) then P := '';
  if P = '' then
    Result := ExpandConstant('{app}')
  else if P[Length(P)] = ';' then
    Result := P + ExpandConstant('{app}')
  else
    Result := P + ';' + ExpandConstant('{app}');
end;

function NeedsPath: Boolean;
var
  P: string;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', P) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Lowercase(ExpandConstant('{app}')) + ';', ';' + Lowercase(P) + ';') = 0;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  P, App: string;
  I: Integer;
begin
  if CurUninstallStep <> usPostUninstall then exit;
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', P) then exit;
  App := ExpandConstant('{app}');
  P := ';' + P + ';';
  I := Pos(';' + Lowercase(App) + ';', Lowercase(P));
  if I > 0 then
  begin
    Delete(P, I, Length(App) + 1);
    P := Copy(P, 2, Length(P) - 2);
    RegWriteExpandStringValue(HKCU, 'Environment', 'Path', P);
  end;
end;
