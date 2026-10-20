# Validate the extracted release and installer in isolated per-user directories.
param([Parameter(Mandatory=$true)][string]$Archive)
$ErrorActionPreference = 'Stop'
$zip = (Resolve-Path -LiteralPath $Archive).Path
$temporary = Join-Path ([IO.Path]::GetTempPath()) ('luma-package-test-' + [Guid]::NewGuid().ToString('N'))
$bundle = Join-Path $temporary 'bundle'
$oldLocalData = $env:LOCALAPPDATA
$oldRoamingData = $env:APPDATA
try {
    Expand-Archive -LiteralPath $zip -DestinationPath $bundle
    $env:LOCALAPPDATA = Join-Path $temporary 'local'
    $env:APPDATA = Join-Path $temporary 'roaming'
    $settings = Join-Path $env:APPDATA 'LumaDesk\layout.json'
    New-Item -ItemType Directory -Path (Split-Path -Parent $settings) -Force | Out-Null
    [IO.File]::WriteAllText($settings, '{"version":3,"fences":[],"history":[{"id":"retained-history"}],"pending":{"retained-intent":true}}')
    $settingsHash = (Get-FileHash -LiteralPath $settings).Hash
    $iconRecovery = Join-Path (Split-Path -Parent $settings) 'icon-session-retained.json'
    [IO.File]::WriteAllText($iconRecovery, '{"owner":0,"flags":4,"icons":[]}')
    $iconRecoveryHash = (Get-FileHash -LiteralPath $iconRecovery).Hash
    $collected = Join-Path $env:APPDATA 'LumaDesk\collections\1\test-item\files\important.txt'
    New-Item -ItemType Directory -Path (Split-Path -Parent $collected) -Force | Out-Null
    [IO.File]::WriteAllText($collected, 'Collected user file must survive uninstall')
    $collectedHash = (Get-FileHash -LiteralPath $collected).Hash
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $bundle 'Install-LumaDesk.ps1') -NoRegistry
    if ($LASTEXITCODE -ne 0) { throw 'Isolated installation failed.' }
    $installed = Join-Path $env:LOCALAPPDATA 'Programs\LumaDesk'
    $manifestPath = Join-Path $installed 'installation.json'
    $manifestText = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8
    $manifest = $manifestText | ConvertFrom-Json
    $version = [IO.File]::ReadAllText((Join-Path $bundle 'VERSION.txt')).Trim()
    if ($manifest.version -ne $version) { throw 'Installed version does not match the ZIP.' }
    foreach ($relative in $manifest.files) {
        $originalHash = (Get-FileHash -LiteralPath (Join-Path $bundle $relative)).Hash
        $installedHash = (Get-FileHash -LiteralPath (Join-Path $installed $relative)).Hash
        if ($originalHash -ne $installedHash) { throw "Installed file does not match: $relative" }
    }
    $shortcut = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\LumaDesk.lnk'
    $shell = New-Object -ComObject WScript.Shell
    if ($shell.CreateShortcut($shortcut).TargetPath -ne (Join-Path $installed 'lumadesk.exe')) {
        throw 'Start menu shortcut has the wrong target.'
    }
    $notes = Join-Path $installed 'docs\my-notes.txt'
    [IO.File]::WriteAllText($notes, 'User-added file must survive uninstall')
    $outside = Join-Path $env:LOCALAPPDATA 'Programs\outside-sentinel.txt'
    [IO.File]::WriteAllText($outside, 'Outside installation')
    $manifest.files = @($manifest.files) + '..\outside-sentinel.txt'
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 4))
    $ErrorActionPreference = 'Continue'
    try {
        $unsafeOutput = & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $installed 'Uninstall-LumaDesk.ps1') -NoRegistry 2>&1
        $unsafeExitCode = $LASTEXITCODE
    } finally { $ErrorActionPreference = 'Stop' }
    if ($unsafeExitCode -eq 0 -or -not (Test-Path -LiteralPath $outside) -or -not (Test-Path -LiteralPath (Join-Path $installed 'lumadesk.exe'))) {
        throw 'Unsafe manifest validation failed to protect files.'
    }
    [IO.File]::WriteAllText($manifestPath, $manifestText)
    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $installed 'Uninstall-LumaDesk.ps1') -NoRegistry
    if ($LASTEXITCODE -ne 0) { throw 'Isolated uninstall failed.' }
    if ((Get-FileHash -LiteralPath $settings).Hash -ne $settingsHash -or -not (Test-Path -LiteralPath $notes)) {
        throw 'Uninstall changed settings or removed user-added files.'
    }
    if ((Get-FileHash -LiteralPath $collected).Hash -ne $collectedHash) { throw 'Uninstall changed a collected user file.' }
    if ((Get-FileHash -LiteralPath $iconRecovery).Hash -ne $iconRecoveryHash) { throw 'Uninstall changed icon recovery data.' }
    $validManifest = $manifestText | ConvertFrom-Json
    foreach ($relative in $validManifest.files) {
        if (Test-Path -LiteralPath (Join-Path $installed $relative)) { throw "Uninstall left a packaged file behind: $relative" }
    }
    if ((Test-Path -LiteralPath (Join-Path $installed 'lumadesk.exe')) -or (Test-Path -LiteralPath $shortcut)) {
        throw 'Uninstall did not remove the program and its own shortcut.'
    }
    Write-Output "PACKAGE_TEST_OK VERSION=$version settings_history_pending=preserved icon_recovery=preserved user_files=preserved unsafe_manifest=rejected"
} finally {
    $env:LOCALAPPDATA = $oldLocalData
    $env:APPDATA = $oldRoamingData
    $tempPrefix = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    $resolved = [IO.Path]::GetFullPath($temporary)
    if ($resolved.StartsWith($tempPrefix, [StringComparison]::OrdinalIgnoreCase) -and
        [IO.Path]::GetFileName($resolved).StartsWith('luma-package-test-')) {
        if (Test-Path -LiteralPath $resolved) { Remove-Item -LiteralPath $resolved -Recurse -Force }
    }
}
