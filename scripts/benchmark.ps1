# Opt-in, reproducible local benchmark. Uses only isolated temporary pins.
# Requires an interactive Explorer desktop with no running LumaDesk.
param(
    [ValidateRange(1,160)][int]$Items = 160,
    [ValidateRange(1,6)][int]$Collections = 1,
    [ValidateRange(5,30)][int]$IdleSeconds = 8,
    [string]$Executable = ""
)

$ErrorActionPreference = 'Stop'
$project = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $Executable) { $Executable = Join-Path $project 'target\release\lumadesk.exe' }
$exe = (Resolve-Path -LiteralPath $Executable).Path
if (@(Get-Process -Name lumadesk -ErrorAction SilentlyContinue | Where-Object { -not $_.HasExited }).Count -gt 0) {
    throw 'Exit LumaDesk before benchmarking.'
}

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class LumaBenchmarkWin32 {
    public delegate bool Visit(IntPtr hwnd, IntPtr context);
    [DllImport("user32.dll")] public static extern IntPtr GetShellWindow();
    [DllImport("user32.dll")] public static extern bool EnumWindows(Visit callback, IntPtr context);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr parent, Visit callback, IntPtr context);
    [DllImport("user32.dll", EntryPoint="FindWindowExW", CharSet=CharSet.Unicode)]
    public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string klass, string title);
    [DllImport("user32.dll", EntryPoint="FindWindowW", CharSet=CharSet.Unicode)]
    public static extern IntPtr FindWindow(string klass, string title);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)]
    public static extern int GetClassName(IntPtr hwnd, StringBuilder text, int capacity);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll", EntryPoint="PostMessageW")]
    public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wparam, IntPtr lparam);

    public static IntPtr DesktopView() {
        var view = FindWindowEx(GetShellWindow(), IntPtr.Zero, "SHELLDLL_DefView", null);
        if (view != IntPtr.Zero && IsWindowVisible(view)) return view;
        IntPtr result = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            var klass = new StringBuilder(80); GetClassName(hwnd, klass, klass.Capacity);
            if (klass.ToString() != "WorkerW") return true;
            var candidate = FindWindowEx(hwnd, IntPtr.Zero, "SHELLDLL_DefView", null);
            if (candidate == IntPtr.Zero || !IsWindowVisible(candidate)) return true;
            result = candidate; return false;
        }, IntPtr.Zero);
        return result;
    }

    public static int CountFences(uint pid) {
        var parent = DesktopView();
        if (parent == IntPtr.Zero) return 0;
        int count = 0;
        EnumChildWindows(parent, (hwnd, _) => {
            uint owner; GetWindowThreadProcessId(hwnd, out owner);
            if (owner != pid || !IsWindowVisible(hwnd)) return true;
            var klass = new StringBuilder(80); GetClassName(hwnd, klass, klass.Capacity);
            if (klass.ToString() == "LumaDeskFenceV1") count++;
            return true;
        }, IntPtr.Zero);
        return count;
    }
}
'@

$temporary = Join-Path ([IO.Path]::GetTempPath()) ('lumadesk-benchmark-' + [Guid]::NewGuid().ToString('N'))
$data = Join-Path $temporary 'LumaDesk'
$samples = Join-Path $temporary 'sample-files'
New-Item -ItemType Directory -Path $data, $samples -Force | Out-Null
$paths = @()
$extensions = @('txt', 'md', 'json', 'csv', 'png')
for ($i = 0; $i -lt $Items; $i++) {
    $path = Join-Path $samples ('Item-{0:D3}.{1}' -f $i, $extensions[$i % $extensions.Length])
    [IO.File]::WriteAllText($path, 'LumaDesk synthetic benchmark pin')
    $paths += $path
}
$fences = @()
for ($i = 0; $i -lt $Collections; $i++) {
    $fences += @{
        id = $i + 1; title = "Benchmark $($i + 1)"; x = 24 + 48 * $i; y = 24 + 48 * $i
        width = 360; height = 420; collapsed = $false; paths = $paths
    }
}
$layout = @{ version = 1; theme = 'midnight'; fences = $fences } | ConvertTo-Json -Depth 5
[IO.File]::WriteAllText((Join-Path $data 'layout.json'), $layout, (New-Object Text.UTF8Encoding $false))

$oldAppData = $env:APPDATA
$proc = $null
try {
    $env:APPDATA = $temporary
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $exe -ArgumentList '--background' -WindowStyle Hidden -PassThru
    $env:APPDATA = $oldAppData
    $readyCount = 0
    for ($attempt = 0; $attempt -lt 250; $attempt++) {
        if ($proc.HasExited) { throw 'Benchmark process exited during startup.' }
        $readyCount = [LumaBenchmarkWin32]::CountFences([uint32]$proc.Id)
        if ($readyCount -eq $Collections) { break }
        Start-Sleep -Milliseconds 20
    }
    $startupMs = $watch.Elapsed.TotalMilliseconds
    if ($readyCount -ne $Collections) {
        throw "Expected $Collections visible fences, got $readyCount."
    }

    Start-Sleep -Seconds 2
    $proc.Refresh()
    $before = $proc.TotalProcessorTime
    $idleWatch = [Diagnostics.Stopwatch]::StartNew()
    Start-Sleep -Seconds $IdleSeconds
    $idleWatch.Stop()
    $proc.Refresh()
    $cpuMs = ($proc.TotalProcessorTime - $before).TotalMilliseconds
    $cpuPercent = 100.0 * $cpuMs / $idleWatch.Elapsed.TotalMilliseconds
    $finalCount = [LumaBenchmarkWin32]::CountFences([uint32]$proc.Id)
    if ($finalCount -ne $Collections) { throw "Window count drifted to $finalCount." }

    Write-Output "BENCHMARK_OK ITEMS_PER_FENCE=$Items FENCES=$Collections"
    Write-Output "STARTUP_TO_ALL_WINDOWS_MS=$([Math]::Round($startupMs,1))"
    Write-Output "IDLE_CPU_PERCENT_ONE_CORE=$([Math]::Round($cpuPercent,3))"
    Write-Output "WORKING_SET_MB=$([Math]::Round($proc.WorkingSet64 / 1MB,1))"
    Write-Output "PRIVATE_MEMORY_MB=$([Math]::Round($proc.PrivateMemorySize64 / 1MB,1))"
} finally {
    $env:APPDATA = $oldAppData
    if ($null -ne $proc -and -not $proc.HasExited) {
        $controller = [LumaBenchmarkWin32]::FindWindow('LumaDeskControllerV1', $null)
        if ($controller -ne [IntPtr]::Zero) {
            [void][LumaBenchmarkWin32]::PostMessage($controller, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
            [void]$proc.WaitForExit(3000)
        }
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    }
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $actual = [IO.Path]::GetFullPath($temporary)
    if ($actual.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
        [IO.Path]::GetFileName($actual).StartsWith('lumadesk-benchmark-')) {
        Remove-Item -LiteralPath $actual -Recurse -Force
    }
}
