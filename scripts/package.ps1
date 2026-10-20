# Creates a portable Windows preview package, suitable for GitHub Releases.
param(
    [string]$OutputDirectory = "",
    [string]$TargetDir = "",
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$project = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $project 'dist' }
$output = [System.IO.Path]::GetFullPath($OutputDirectory)
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    $candidate = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (-not (Test-Path -LiteralPath $candidate)) { throw 'Rust Cargo is required to package LumaDesk.' }
    $cargo = $candidate
} else {
    $cargo = $cargo.Source
}

Push-Location $project
try {
    $version = (Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
    if (-not $version) { throw 'Could not read Cargo package version.' }
    # -TargetDir builds elsewhere when target\release is running.
    if (-not $TargetDir) { $TargetDir = Join-Path $project 'target' }
    $exe = Join-Path $TargetDir 'release\lumadesk.exe'
    if (-not $SkipBuild) {
        $running = @(Get-Process -Name lumadesk -ErrorAction SilentlyContinue | Where-Object { -not $_.HasExited -and $_.Path -eq $exe })
        if ($running.Count -gt 0) {
            throw 'LumaDesk release binary is currently running. Exit from its tray menu, or pass -TargetDir.'
        }
        & $cargo build --release --locked --target-dir $TargetDir
        if ($LASTEXITCODE -ne 0) { throw 'Release compilation failed.' }
    }
    if (-not (Test-Path -LiteralPath $exe)) { throw 'Release executable does not exist.' }

    New-Item -ItemType Directory -Path $output -Force | Out-Null
    $packageName = "LumaDesk-$version-windows-x64"
    $zip = Join-Path $output "$packageName.zip"
    $sourceFiles = @(
        $exe,
        (Join-Path $project 'README.md'),
        (Join-Path $project 'README.zh-TW.md'),
        (Join-Path $project 'CHANGELOG.md'),
        (Join-Path $project 'LICENSE'),
        (Join-Path $project 'scripts\Install-LumaDesk.ps1'),
        (Join-Path $project 'scripts\Uninstall-LumaDesk.ps1'),
        (Join-Path $project 'assets'),
        (Join-Path $project 'docs')
    )
    $stage = Join-Path $output ('package-stage-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
        foreach ($source in $sourceFiles) {
            Copy-Item -LiteralPath $source -Destination $stage -Recurse -Force
        }
        Set-Content -LiteralPath (Join-Path $stage 'VERSION.txt') -Value $version -Encoding ascii
        $stagedFiles = @(Get-ChildItem -LiteralPath $stage | ForEach-Object { $_.FullName })
        Compress-Archive -LiteralPath $stagedFiles -DestinationPath $zip -CompressionLevel Optimal -Force
    } finally {
        $stageFull = [IO.Path]::GetFullPath($stage)
        $outputPrefix = $output.TrimEnd('\') + '\'
        if ($stageFull.StartsWith($outputPrefix, [StringComparison]::OrdinalIgnoreCase) -and
            [IO.Path]::GetFileName($stageFull).StartsWith('package-stage-')) {
            Remove-Item -LiteralPath $stageFull -Recurse -Force
        }
    }

    Add-Type -AssemblyName System.IO.Compression
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $names = @($archive.Entries | ForEach-Object { $_.FullName.Replace('\','/') })
        foreach ($required in @('lumadesk.exe','VERSION.txt','Install-LumaDesk.ps1','Uninstall-LumaDesk.ps1','README.md','README.zh-TW.md','CHANGELOG.md','LICENSE','assets/lumadesk.png','assets/preview.png','docs/PERFORMANCE.md','docs/操作說明.zh-TW.md')) {
            if ($names -notcontains $required) { throw "Package is missing $required" }
        }
    } finally {
        $archive.Dispose()
    }
    $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    $checksums = Join-Path $output 'SHA256SUMS.txt'
    Set-Content -LiteralPath $checksums -Value "$hash  $packageName.zip" -Encoding ascii
    Write-Output "PACKAGE=$zip"
    Write-Output "SHA256=$hash"
    Write-Output "SIZE_BYTES=$((Get-Item -LiteralPath $zip).Length)"
} finally {
    Pop-Location
}
