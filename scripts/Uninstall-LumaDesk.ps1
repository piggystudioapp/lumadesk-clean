# Remove per-user installed program files; preserve %APPDATA%\LumaDesk layouts.
param([switch]$NoRegistry)

$ErrorActionPreference = 'Stop'
$destination = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Programs\LumaDesk'))
$installedExe = Join-Path $destination 'lumadesk.exe'
$manifestPath = Join-Path $destination 'installation.json'
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "LumaDesk installation manifest was not found: $manifestPath"
}
if (@(Get-Process -Name lumadesk -ErrorAction SilentlyContinue | Where-Object { -not $_.HasExited }).Count -gt 0) {
    throw 'Exit LumaDesk from its system tray before uninstalling.'
}

$manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
if (-not $manifest.files -or -not $manifest.version) {
    throw 'Installation manifest is incomplete; no files were deleted.'
}
$prefix = $destination.TrimEnd('\') + '\'
$targets = @()
foreach ($relative in $manifest.files) {
    $relative = [string]$relative
    if ([IO.Path]::IsPathRooted($relative)) { throw 'Unsafe absolute path in manifest.' }
    $full = [IO.Path]::GetFullPath((Join-Path $destination $relative))
    if (-not $full.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Unsafe path in manifest; no files were deleted.'
    }
    $targets += $full
}

$startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\LumaDesk.lnk'
if (Test-Path -LiteralPath $startMenu -PathType Leaf) {
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($startMenu)
    if ([string]::Equals($shortcut.TargetPath, $installedExe, [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $startMenu -Force
    }
}

if (-not $NoRegistry) {
    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    $startup = (Get-ItemProperty -Path $runKey -Name LumaDesk -ErrorAction SilentlyContinue).LumaDesk
    if ([string]::Equals($startup, ('"' + $installedExe + '"'), [StringComparison]::OrdinalIgnoreCase) -or
        [string]::Equals($startup, ('"' + $installedExe + '" --background'), [StringComparison]::OrdinalIgnoreCase)) {
        Remove-ItemProperty -Path $runKey -Name LumaDesk -ErrorAction SilentlyContinue
    }
    $key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\LumaDesk'
    $entry = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
    if ($entry -and [string]::Equals($entry.InstallLocation, $destination, [StringComparison]::OrdinalIgnoreCase)) {
        Remove-Item -LiteralPath $key -Force
    }
}

# Only remove files recorded by this installation. User-added files survive.
foreach ($target in $targets) {
    if (Test-Path -LiteralPath $target -PathType Leaf) {
        Remove-Item -LiteralPath $target -Force
    }
}
Remove-Item -LiteralPath $manifestPath -Force
foreach ($folder in @('assets', 'docs')) {
    $path = Join-Path $destination $folder
    if ((Test-Path -LiteralPath $path -PathType Container) -and
        @(Get-ChildItem -LiteralPath $path -Force).Count -eq 0) {
        Remove-Item -LiteralPath $path
    }
}
if (@(Get-ChildItem -LiteralPath $destination -Force).Count -eq 0) {
    Remove-Item -LiteralPath $destination
}
Write-Output "UNINSTALLED=$destination"
Write-Output 'SETTINGS_PRESERVED=true'
