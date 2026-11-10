# Hurtle installer for Windows (per user, no admin rights).
#   irm https://arafays.github.io/hurtle/install.ps1 | iex
# Pin a version:  $env:HURTLE_VERSION = '0.2.2'; irm .../install.ps1 | iex
# Uninstall:      & ([scriptblock]::Create((irm https://arafays.github.io/hurtle/install.ps1))) -Uninstall
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = 'arafays/hurtle'
$Dir = Join-Path $env:LOCALAPPDATA 'Programs\Hurtle'
$Shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Hurtle.lnk'

# The user PATH is read and written straight from the registry: the .NET
# environment API expands %VAR% entries and would store the result as REG_SZ.
function Get-UserPathRaw {
    (Get-Item 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames')
}
function Set-UserPathRaw([string]$value) {
    Set-ItemProperty -Path 'HKCU:\Environment' -Name Path -Value $value -Type ExpandString
}

function Remove-FromUserPath([string]$entry) {
    $cur = Get-UserPathRaw
    if (-not $cur) { return }
    $keep = $cur.Split(';') | Where-Object { $_ -and ($_.TrimEnd('\') -ne $entry.TrimEnd('\')) }
    Set-UserPathRaw ($keep -join ';')
}

function Stop-Hurtle {
    Get-Process -Name hurtle, hurtle-host -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($Dir, [StringComparison]::OrdinalIgnoreCase) } |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
}

if ($Uninstall) {
    $host_exe = Join-Path $Dir 'hurtle-host.exe'
    if (Test-Path $host_exe) { & $host_exe --unregister | Out-Null }
    Stop-Hurtle
    if (Test-Path $Shortcut) { Remove-Item -Force $Shortcut }
    Remove-FromUserPath $Dir
    if (Test-Path $Dir) { Remove-Item -Recurse -Force $Dir }
    Write-Host 'Hurtle removed. Your downloads and settings in %APPDATA% were left alone.'
    return
}

if (-not [Environment]::Is64BitOperatingSystem) { throw 'Hurtle needs 64-bit Windows.' }
if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { Write-Host 'ARM64: installing the x86_64 build (runs under emulation).' }

$version = $env:HURTLE_VERSION
if ($version) {
    $version = $version.TrimStart('v')
} else {
    $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'hurtle-install' }
    $version = $rel.tag_name.TrimStart('v')
}
$name = "hurtle-$version-windows-x86_64"
$base = "https://github.com/$Repo/releases/download/v$version"

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("hurtle-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading Hurtle $version ..."
    $zip = Join-Path $tmp "$name.zip"
    Invoke-WebRequest "$base/$name.zip" -OutFile $zip -UseBasicParsing
    $sums = Join-Path $tmp 'SHA256SUMS.txt'
    Invoke-WebRequest "$base/SHA256SUMS.txt" -OutFile $sums -UseBasicParsing

    $line = Get-Content $sums | Where-Object { $_ -match "\s\*?$([regex]::Escape("$name.zip"))$" } | Select-Object -First 1
    if (-not $line) { throw "$name.zip is not listed in SHA256SUMS.txt" }
    $want = ($line -split '\s+')[0].ToLower()
    $got = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
    if ($want -ne $got) { throw "Checksum mismatch for $name.zip (expected $want, got $got)" }

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $src = Join-Path $tmp $name
    if (-not (Test-Path (Join-Path $src 'hurtle.exe'))) { throw 'hurtle.exe not found in the archive' }

    Stop-Hurtle
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    Copy-Item -Force (Join-Path $src '*') $Dir
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$ws = New-Object -ComObject WScript.Shell
$sc = $ws.CreateShortcut($Shortcut)
$sc.TargetPath = Join-Path $Dir 'hurtle.exe'
$sc.WorkingDirectory = $Dir
$sc.Description = 'Hurtle download manager'
$sc.Save()

$cur = Get-UserPathRaw
if (-not $cur -or -not ($cur.Split(';') | Where-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -eq $Dir.TrimEnd('\') })) {
    Set-UserPathRaw (($cur, $Dir | Where-Object { $_ }) -join ';')
}

& (Join-Path $Dir 'hurtle-host.exe') --register | Out-Null

Write-Host "Hurtle $version installed to $Dir."
Write-Host 'Start it from the Start menu, then install the browser extension from the release page:'
Write-Host "  https://github.com/$Repo/releases/tag/v$version"
Write-Host 'Open a new terminal to use hurtle-cli.'
