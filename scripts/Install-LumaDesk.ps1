# Per-user installer for the extracted LumaDesk portable ZIP.
# No administrator rights, services, or downloaded dependencies required.
param([switch]$Launch, [switch]$NoRegistry)

$ErrorActionPreference = 'Stop'
$bundle = $PSScriptRoot
$versionFile = Join-Path $bundle 'VERSION.txt'
$exe = Join-Path $bundle 'lumadesk.exe'
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw 'Extract the full LumaDesk ZIP before running Install-LumaDesk.ps1.'
}
if (-not (Test-Path -LiteralPath $versionFile -PathType Leaf)) {
    throw 'VERSION.txt is missing from the extracted package.'
}
$version = [IO.File]::ReadAllText($versionFile).Trim()
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'Invalid packaged version.' }
if (@(Get-Process -Name lumadesk -ErrorAction SilentlyContinue | Where-Object { -not $_.HasExited }).Count -gt 0) {
    throw 'Exit LumaDesk from its system tray before installing or upgrading.'
}

$destination = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Programs\LumaDesk'))
$installedExe = Join-Path $destination 'lumadesk.exe'
$payload = @(
    'lumadesk.exe', 'VERSION.txt', 'README.md', 'README.zh-TW.md',
    'CHANGELOG.md', 'LICENSE', 'Uninstall-LumaDesk.ps1'
)
foreach ($folder in @('assets', 'docs')) {
    $sourceFolder = Join-Path $bundle $folder
    if (Test-Path -LiteralPath $sourceFolder -PathType Container) {
        $rootPrefix = [IO.Path]::GetFullPath($bundle).TrimEnd('\') + '\'
        $payload += @(Get-ChildItem -LiteralPath $sourceFolder -File -Recurse |
            ForEach-Object { $_.FullName.Substring($rootPrefix.Length) })
    }
}

# Validate the entire bundle before changing the installed copy.
foreach ($relative in $payload) {
    if (-not (Test-Path -LiteralPath (Join-Path $bundle $relative) -PathType Leaf)) {
        throw "Package file is missing: $relative"
    }
}

New-Item -ItemType Directory -Path $destination -Force | Out-Null
foreach ($relative in $payload) {
    $target = Join-Path $destination $relative
    $parent = Split-Path -Parent $target
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $bundle $relative) -Destination $target -Force
}
$manifest = @{ version = $version; files = @($payload) } | ConvertTo-Json -Depth 3
[IO.File]::WriteAllText((Join-Path $destination 'installation.json'), $manifest, (New-Object Text.UTF8Encoding $false))

$startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
New-Item -ItemType Directory -Path $startMenu -Force | Out-Null
$shortcutPath = Join-Path $startMenu 'LumaDesk.lnk'
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $installedExe
$shortcut.WorkingDirectory = $destination
$shortcut.IconLocation = "$installedExe,0"
$shortcut.Description = 'LumaDesk desktop organizer'
$shortcut.Save()

if (-not $NoRegistry) {
    $key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\LumaDesk'
    New-Item -Path $key -Force | Out-Null
    $fields = @{
        DisplayName = 'LumaDesk'
        DisplayVersion = $version
        Publisher = 'LumaDesk Contributors'
        InstallLocation = $destination
        DisplayIcon = "$installedExe,0"
        UninstallString = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $destination 'Uninstall-LumaDesk.ps1')`""
    }
    foreach ($field in $fields.Keys) {
        New-ItemProperty -Path $key -Name $field -PropertyType String -Value $fields[$field] -Force | Out-Null
    }
    New-ItemProperty -Path $key -Name NoModify -PropertyType DWord -Value 1 -Force | Out-Null
    New-ItemProperty -Path $key -Name NoRepair -PropertyType DWord -Value 1 -Force | Out-Null
}

Write-Output "INSTALLED=$destination"
Write-Output "VERSION=$version"
Write-Output "START_MENU_SHORTCUT=$shortcutPath"
if ($Launch) {
    Start-Process -FilePath $installedExe -WindowStyle Hidden
}
