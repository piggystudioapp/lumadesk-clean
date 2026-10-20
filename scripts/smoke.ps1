# Windows desktop-shell smoke test. Runs against isolated temporary app data.
# Requires an interactive Explorer desktop and no other running LumaDesk.
param([string]$Executable = "", [string]$ScreenshotPath = "", [string]$OleProbe = "")

$ErrorActionPreference = 'Stop'
$project = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $Executable) { $Executable = Join-Path $project 'target\release\lumadesk.exe' }
$exe = (Resolve-Path -LiteralPath $Executable).Path
if (@(Get-Process -Name lumadesk -ErrorAction SilentlyContinue | Where-Object { -not $_.HasExited }).Count -ne 0) {
    throw 'Exit any running LumaDesk from the tray before smoke testing.'
}
if (-not $OleProbe) {
    $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    Push-Location $project
    try {
        & $cargo build --examples --locked
        if ($LASTEXITCODE -ne 0) { throw 'OLE regression probe compilation failed.' }
    } finally { Pop-Location }
    $OleProbe = Join-Path $project 'target\debug\examples\ole_drop_probe.exe'
}
$OleProbe = (Resolve-Path -LiteralPath $OleProbe).Path

$source = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class LumaSmoke {
    public delegate bool Visit(IntPtr hwnd, IntPtr context);
    [DllImport("shell32.dll",CharSet=CharSet.Unicode)] public static extern void SHChangeNotify(uint events,uint flags,string path,IntPtr other);
    [StructLayout(LayoutKind.Sequential)] public struct Rect {
        public int Left, Top, Right, Bottom;
    }
    [DllImport("user32.dll")] public static extern IntPtr GetShellWindow();
    [DllImport("user32.dll")] public static extern bool EnumWindows(Visit callback, IntPtr context);
    [DllImport("user32.dll", EntryPoint="FindWindowExW", CharSet=CharSet.Unicode)]
    public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string klass, string title);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)]
    public static extern int GetClassName(IntPtr hwnd, StringBuilder value, int capacity);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr hwnd);
    public static void WaitIdle(IntPtr launcher) { for(int n=0;n<150;n++) { if(IsWindowEnabled(GetDlgItem(launcher,1001))) return; System.Threading.Thread.Sleep(100); } throw new Exception("Item worker did not become idle"); }
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll", EntryPoint="PostMessageW")]
    public static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wparam, IntPtr lparam);
    [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr hwnd, int id);
    [DllImport("user32.dll", EntryPoint="SendMessageW")]
    public static extern IntPtr SendMessage(IntPtr hwnd, uint message, IntPtr wparam, IntPtr lparam);
    [DllImport("user32.dll", EntryPoint="SendMessageW", CharSet=CharSet.Unicode)]
    private static extern IntPtr ListText(IntPtr hwnd, uint message, IntPtr wparam, StringBuilder text);
    public static void SelectAutoFixtures(IntPtr preview, string prefix, int expected) {
        var list = GetDlgItem(preview, 1101);
        if (list == IntPtr.Zero) throw new Exception("Preview list missing");
        SendMessage(list, 0x0185, IntPtr.Zero, new IntPtr(-1));
        int count = SendMessage(list, 0x018B, IntPtr.Zero, IntPtr.Zero).ToInt32();
        int selected = 0;
        for (int i = 0; i < count; i++) {
            int length = SendMessage(list, 0x018A, new IntPtr(i), IntPtr.Zero).ToInt32();
            if (length < 0 || length > 32768) throw new Exception("Invalid preview row length");
            var text = new StringBuilder(length + 1);
            ListText(list, 0x0189, new IntPtr(i), text);
            string value = text.ToString();
            int marker = value.IndexOf('】');
            if (marker >= 0 && value.Substring(marker + 1).StartsWith(prefix, StringComparison.Ordinal)) {
                SendMessage(list, 0x0185, new IntPtr(1), new IntPtr(i));
                selected++;
            }
        }
        if (selected != expected) throw new Exception("Unexpected automatic fixture selection: " + selected);
        if (SendMessage(list, 0x0190, IntPtr.Zero, IntPtr.Zero).ToInt32() != expected)
            throw new Exception("Selection count mismatch");
    }
    [DllImport("kernel32.dll", SetLastError=true)] private static extern IntPtr GlobalAlloc(uint flags, UIntPtr size);
    [DllImport("kernel32.dll", SetLastError=true)] private static extern IntPtr GlobalLock(IntPtr handle);
    [DllImport("kernel32.dll")] private static extern bool GlobalUnlock(IntPtr handle);
    [DllImport("kernel32.dll")] private static extern IntPtr GlobalFree(IntPtr handle);

    // Legacy WM_DROPFILES parser coverage only; genuine OLE is tested separately.
    // The receiving window owns the HGLOBAL after a successful PostMessage.
    public static bool SendFileDrop(IntPtr hwnd, string fullPath) {
        byte[] filenames = Encoding.Unicode.GetBytes(fullPath + "\0\0");
        IntPtr handle = GlobalAlloc(0x42, new UIntPtr((uint)(20 + filenames.Length)));
        if (handle == IntPtr.Zero) return false;
        IntPtr pointer = GlobalLock(handle);
        if (pointer == IntPtr.Zero) { GlobalFree(handle); return false; }
        Marshal.WriteInt32(pointer, 0, 20);  // DROPFILES.pFiles
        Marshal.WriteInt32(pointer, 16, 1); // DROPFILES.fWide
        Marshal.Copy(filenames, 0, IntPtr.Add(pointer, 20), filenames.Length);
        GlobalUnlock(handle);
        if (PostMessage(hwnd, 0x0233, handle, IntPtr.Zero)) return true;
        GlobalFree(handle);
        return false;
    }

    public static IntPtr Controller(uint processId) {
        return WindowOfClass(processId, "LumaDeskControllerV1");
    }
    public static IntPtr Launcher(uint processId) {
        return WindowOfClass(processId, "LumaDeskLauncherV1");
    }
    public static IntPtr WindowOfClass(uint processId, string className) {
        IntPtr result = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            uint owner; GetWindowThreadProcessId(hwnd, out owner);
            if (owner != processId) return true;
            var klass = new StringBuilder(80); GetClassName(hwnd, klass, klass.Capacity);
            if (klass.ToString() != className) return true;
            result = hwnd; return false;
        }, IntPtr.Zero);
        return result;
    }

    public static IntPtr DesktopView() {
        IntPtr result = FindWindowEx(GetShellWindow(), IntPtr.Zero, "SHELLDLL_DefView", null);
        if (result != IntPtr.Zero && IsWindowVisible(result)) return result;
        result = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            var klass = new StringBuilder(80); GetClassName(hwnd, klass, klass.Capacity);
            if (klass.ToString() != "WorkerW") return true;
            var view = FindWindowEx(hwnd, IntPtr.Zero, "SHELLDLL_DefView", null);
            if (view == IntPtr.Zero || !IsWindowVisible(view)) return true;
            result = view; return false;
        }, IntPtr.Zero);
        return result;
    }

    public static Rect[] Fences(uint processId) {
        var result = new System.Collections.Generic.List<Rect>();
        var view = DesktopView();
        IntPtr after = IntPtr.Zero;
        for (int i = 0; i < 100 && view != IntPtr.Zero; i++) {
            var hwnd = FindWindowEx(view, after, "LumaDeskFenceV1", null);
            if (hwnd == IntPtr.Zero) break;
            uint owner; GetWindowThreadProcessId(hwnd, out owner);
            Rect rect;
            if (owner == processId && GetWindowRect(hwnd, out rect)) result.Add(rect);
            after = hwnd;
        }
        return result.ToArray();
    }
    public static IntPtr FirstFence() {
        return FindWindowEx(DesktopView(), IntPtr.Zero, "LumaDeskFenceV1", null);
    }
    public static IntPtr OtherFence(uint processId, IntPtr excluded) {
        IntPtr after = IntPtr.Zero;
        for (int i = 0; i < 100; i++) {
            var hwnd = FindWindowEx(DesktopView(), after, "LumaDeskFenceV1", null);
            if (hwnd == IntPtr.Zero) break;
            uint owner; GetWindowThreadProcessId(hwnd, out owner);
            if (hwnd != excluded && owner == processId) return hwnd;
            after = hwnd;
        }
        return IntPtr.Zero;
    }
}
'@
Add-Type -TypeDefinition $source
Add-Type -AssemblyName System.Windows.Forms

$temporary = Join-Path ([IO.Path]::GetTempPath()) ('lumadesk-smoke-' + [Guid]::NewGuid().ToString('N'))
$configDirectory = Join-Path $temporary 'LumaDesk'
New-Item -ItemType Directory -Path $configDirectory -Force | Out-Null
$layout = @{
    version = 1
    theme = 'midnight'
    fences = @(@{ id=1; title='Smoke test'; x=50000; y=50000; width=360; height=420; collapsed=$false; list_view=$true; paths=@((Join-Path $project 'Cargo.toml'), (Join-Path $project 'README.md'), (Join-Path $project 'LICENSE')) })
} | ConvertTo-Json -Depth 5
[IO.File]::WriteAllText((Join-Path $configDirectory 'layout.json'), $layout, (New-Object Text.UTF8Encoding $false))

$proc = $null
$oldAppData = $env:APPDATA
$oldSmokePid = $env:LUMADESK_SMOKE_PID
$desktopFixtures = @()
try {
    $env:APPDATA = $temporary
    $proc = Start-Process -FilePath $exe -WindowStyle Hidden -PassThru
    $env:APPDATA = $oldAppData
    $controller = [IntPtr]::Zero
    $fences = @()
    for ($try=0; $try -lt 30; $try++) {
        if ($proc.HasExited) { throw 'LumaDesk exited during startup.' }
        $controller = [LumaSmoke]::Controller([uint32]$proc.Id)
        $fences = @([LumaSmoke]::Fences([uint32]$proc.Id))
        if ($controller -ne [IntPtr]::Zero -and $fences.Count -eq 1) { break }
        Start-Sleep -Milliseconds 150
    }
    if ($fences.Count -ne 1) { throw "Expected 1 fence, found $($fences.Count)." }
    $launcher = [IntPtr]::Zero
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        $launcher = [LumaSmoke]::Launcher([uint32]$proc.Id)
        if ($launcher -ne [IntPtr]::Zero -and [LumaSmoke]::IsWindowVisible($launcher)) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($launcher -eq [IntPtr]::Zero -or -not [LumaSmoke]::IsWindowVisible($launcher)) {
        throw 'A normal launch did not show the LumaDesk launcher.'
    }
    [void][LumaSmoke]::PostMessage($launcher, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
    for ($attempt = 0; $attempt -lt 30 -and [LumaSmoke]::IsWindowVisible($launcher); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if ($proc.HasExited -or [LumaSmoke]::IsWindowVisible($launcher)) {
        throw 'Closing the launcher must hide it while desktop collections keep running.'
    }
    $working = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea
    $rect = $fences[0]
    if ($rect.Left -lt $working.Left -or $rect.Top -lt $working.Top -or
        $rect.Right -gt $working.Right -or $rect.Bottom -gt $working.Bottom) {
        throw 'Offscreen recovery did not place the fence inside the primary work area.'
    }
    if ($ScreenshotPath) {
        Add-Type -AssemblyName System.Drawing
        $fenceForCapture = [LumaSmoke]::FirstFence()
        Start-Sleep -Milliseconds 400
        $bitmap = New-Object System.Drawing.Bitmap(($rect.Right - $rect.Left)), (($rect.Bottom - $rect.Top))
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        try {
            $dc = $graphics.GetHdc()
            try {
                if (-not [LumaSmoke]::PrintWindow($fenceForCapture, $dc, 2)) {
                    throw 'PrintWindow failed while capturing the collection.'
                }
            } finally {
                $graphics.ReleaseHdc($dc)
            }
            $absoluteScreenshot = [IO.Path]::GetFullPath($ScreenshotPath)
            $bitmap.Save($absoluteScreenshot, [System.Drawing.Imaging.ImageFormat]::Png)
            Write-Output "SCREENSHOT=$absoluteScreenshot"
        } finally {
            $graphics.Dispose()
            $bitmap.Dispose()
        }
    }
    Start-Sleep -Seconds 7 # at least two controller refresh timer ticks
    if (@([LumaSmoke]::Fences([uint32]$proc.Id)).Count -ne 1) {
        throw 'A duplicate collection window appeared after timer refresh.'
    }
    $fence = [LumaSmoke]::FirstFence()
    if ($fence -eq [IntPtr]::Zero) { throw 'Fence window handle unavailable.' }
    if (-not [LumaSmoke]::PostMessage($fence, 0x0100, [IntPtr]0x28, [IntPtr]::Zero)) {
        throw 'Could not send keyboard Down.'
    }
    if (-not [LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1004, [IntPtr]::Zero)) {
        throw 'Could not send the remove-from-group command.'
    }
    Start-Sleep -Milliseconds 400
    $saved = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($saved.fences[0].paths.Count -ne 2 -or $saved.fences[0].paths[0] -ne (Join-Path $project 'README.md') -or $saved.fences[0].paths[1] -ne (Join-Path $project 'LICENSE')) {
        throw 'Keyboard selection and unpin did not persist the expected item.'
    }
    # Activate Find through the same command used by the collection menu,
    # filter down to LICENSE, and delete only that filtered item.
    if (-not [LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1012, [IntPtr]::Zero)) {
        throw 'Could not activate collection search.'
    }
    foreach ($code in @(76, 73, 67)) {
        if (-not [LumaSmoke]::PostMessage($fence, 0x0102, [IntPtr]$code, [IntPtr]::Zero)) {
            throw 'Could not type search text.'
        }
    }
    [void][LumaSmoke]::PostMessage($fence, 0x0100, [IntPtr]0x24, [IntPtr]::Zero)
    if (-not [LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1004, [IntPtr]::Zero)) {
        throw 'Could not delete the filtered item.'
    }
    Start-Sleep -Milliseconds 400
    $filtered = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($filtered.fences[0].paths.Count -ne 1 -or $filtered.fences[0].paths[0] -ne (Join-Path $project 'README.md')) {
        throw 'Search result selection did not target the correct item.'
    }
    # Return to the unfiltered view, then exercise the actual WM_DROPFILES handler.
    [void][LumaSmoke]::PostMessage($fence, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)
    $draggedPath = Join-Path $temporary 'dragged-from-explorer.txt'
    [IO.File]::WriteAllText($draggedPath, 'Synthetic desktop smoke file')
    if (-not [LumaSmoke]::SendFileDrop($fence, $draggedPath)) {
        throw 'Could not send the synthetic file drop.'
    }
    Start-Sleep -Milliseconds 400
    $dropped = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($dropped.fences[0].paths.Count -ne 2 -or $dropped.fences[0].paths[1] -ne $draggedPath) {
        throw 'WM_DROPFILES did not pin and persist the dropped file.'
    }
    # Reorder through the window commands used by the collection context menu.
    [void][LumaSmoke]::PostMessage($fence, 0x0100, [IntPtr]0x24, [IntPtr]::Zero)
    [void][LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1014, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 300
    $movedDown = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($movedDown.fences[0].paths[0] -ne $draggedPath -or $movedDown.fences[0].paths[1] -ne (Join-Path $project 'README.md')) {
        throw 'Move down did not persist the reordered pins.'
    }
    [void][LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1013, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 300
    $movedUp = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($movedUp.fences[0].paths[0] -ne (Join-Path $project 'README.md') -or $movedUp.fences[0].paths[1] -ne $draggedPath) {
        throw 'Move up did not restore the original pin order.'
    }
    # Outbound mouse selection/reorder/cancel cases use a topmost isolated host
    # in arrange_probe, so unrelated foreground windows cannot cover the drop.
    $headerClick = [IntPtr]((20 -shl 16) -bor 60)
    if (-not [LumaSmoke]::PostMessage($fence, 0x0203, [IntPtr]::Zero, $headerClick)) {
        throw 'Could not collapse the collection.'
    }
    Start-Sleep -Milliseconds 400
    $collapsed = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if (-not $collapsed.fences[0].collapsed) { throw 'Collapse did not persist.' }
    if (-not [LumaSmoke]::PostMessage($fence, 0x0203, [IntPtr]::Zero, $headerClick)) {
        throw 'Could not expand the collection.'
    }
    Start-Sleep -Milliseconds 400
    $expanded = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($expanded.fences[0].collapsed) { throw 'Expansion did not persist.' }

    # Exercise the Windows OLE drag loop against this process's actual drop
    # target with Explorer's Shell IDataObject, not a posted WM_DROPFILES.
    $env:LUMADESK_SMOKE_PID = [string]$proc.Id
    function Invoke-OleProbe([string]$Mode, [string[]]$Paths) {
        # The native probe synchronizes the controller's refresh timer after
        # creating Shell data and before temporarily hosting the collection.
        $script:fence = [LumaSmoke]::FirstFence()
        if ($script:fence -eq [IntPtr]::Zero) { throw 'Isolated collection HWND unavailable before OLE probe.' }
        $arguments = @([string]$fence.ToInt64(), $Mode) + $Paths
        $quoted = @($arguments | ForEach-Object { '"' + $_ + '"' })
        $startInfo = New-Object Diagnostics.ProcessStartInfo
        $startInfo.FileName = $OleProbe
        $startInfo.Arguments = $quoted -join ' '
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        $probeProcess = New-Object Diagnostics.Process
        $probeProcess.StartInfo = $startInfo
        [void]$probeProcess.Start()
        if (-not $probeProcess.WaitForExit(10000)) {
            Stop-Process -Id $probeProcess.Id -Force
            [void]$probeProcess.WaitForExit(2000)
            throw "OLE $Mode probe timed out: $($probeProcess.StandardError.ReadToEnd())"
        }
        $probeOutput = $probeProcess.StandardOutput.ReadToEnd()
        $probeError = $probeProcess.StandardError.ReadToEnd()
        if ($probeProcess.ExitCode -ne 0) {
            throw "OLE $Mode probe failed ($($probeProcess.ExitCode)): $probeError $probeOutput"
        }
        $probeOutput.Trim()
        [LumaSmoke]::WaitIdle($launcher)
        Start-Sleep -Milliseconds 500
        $script:fence = [LumaSmoke]::FirstFence()
        for ($attempt = 0; $attempt -lt 25 -and $script:fence -eq [IntPtr]::Zero; $attempt++) {
            Start-Sleep -Milliseconds 150
            $script:fence = [LumaSmoke]::FirstFence()
        }
        if ($script:fence -eq [IntPtr]::Zero) { throw 'The isolated collection did not return to the desktop after OLE testing.' }
    }
    $unicodePath = Join-Path $temporary (([string][char]0x62D6) + [char]0x5165 + '-ole.txt')
    [IO.File]::WriteAllText($unicodePath, 'OLE pinning leaves source data intact')
    $sourceHash = (Get-FileHash -LiteralPath $unicodePath -Algorithm SHA256).Hash
    foreach ($mode in @('cancel', 'move')) {
        Invoke-OleProbe $mode @($unicodePath)
        $unchanged = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($unchanged.fences[0].paths.Count -ne 2) { throw "OLE $mode unexpectedly changed pins." }
    }
    $oleFolder = Join-Path $temporary 'ole-folder'
    New-Item -ItemType Directory -Path $oleFolder | Out-Null
    $oleShortcut = Join-Path $temporary 'ole-shortcut.lnk'
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($oleShortcut)
    $shortcut.TargetPath = $unicodePath
    $shortcut.Save()
    # Clear an active search and expand a collapsed collection on arrival.
    [void][LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1012, [IntPtr]::Zero)
    [void][LumaSmoke]::PostMessage($fence, 0x0102, [IntPtr]90, [IntPtr]::Zero)
    Invoke-OleProbe 'link' @($unicodePath, $oleFolder, $oleShortcut)
    $oleSaved = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    foreach ($expected in @($unicodePath, $oleFolder, $oleShortcut)) {
        if ($oleSaved.fences[0].paths -notcontains $expected -or -not (Test-Path -LiteralPath $expected)) {
            throw "OLE did not pin and preserve source: $expected"
        }
    }
    if ($oleSaved.fences[0].paths.Count -ne 5) { throw 'OLE multi-file drop did not persist all three paths.' }
    if ((Get-FileHash -LiteralPath $unicodePath -Algorithm SHA256).Hash -ne $sourceHash) {
        throw 'OLE pinning changed source content.'
    }
    # With the search cleared, Home/Delete must target the original first row.
    [void][LumaSmoke]::PostMessage($fence, 0x0100, [IntPtr]0x24, [IntPtr]::Zero)
    [void][LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1004, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 250
    $afterSearch = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($afterSearch.fences[0].paths.Count -ne 4 -or $afterSearch.fences[0].paths -contains (Join-Path $project 'README.md')) {
        throw 'OLE drop did not clear the search to expose the new pins.'
    }
    $copyPath = Join-Path $temporary 'copy-only.txt'
    [IO.File]::WriteAllText($copyPath, 'Copy effect is still a reference')
    [void][LumaSmoke]::PostMessage($fence, 0x0203, [IntPtr]::Zero, $headerClick)
    Start-Sleep -Milliseconds 300
    Invoke-OleProbe 'copy' @($copyPath)
    Start-Sleep -Milliseconds 300
    $copySaved = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($copySaved.fences[0].paths -notcontains $copyPath -or $copySaved.fences[0].collapsed -or -not (Test-Path -LiteralPath $copyPath)) {
        throw 'Copy-only OLE source was not pinned, preserved, and expanded.'
    }
    # Only GUID-named fixtures are created on the real desktop. All storage
    # and layout changes remain isolated in the test APPDATA directory.
    $desktopRoot = [Environment]::GetFolderPath('DesktopDirectory')
    $fixtureName = 'LumaDesk-Smoke-' + [Guid]::NewGuid().ToString('N')
    $desktopFile = Join-Path $desktopRoot ($fixtureName + '.txt')
    $desktopFolder = Join-Path $desktopRoot ($fixtureName + '-folder')
    $desktopShortcut = Join-Path $desktopRoot ($fixtureName + '.lnk')
    $desktopFixtures = @($desktopFile, $desktopFolder, $desktopShortcut)
    [IO.File]::WriteAllText($desktopFile, 'Desktop storage preserves content')
    New-Item -ItemType Directory -Path $desktopFolder | Out-Null
    [IO.File]::WriteAllText((Join-Path $desktopFolder 'nested.txt'), 'nested content')
    $desktopLink = $shell.CreateShortcut($desktopShortcut)
    $desktopLink.TargetPath = $copyPath
    $desktopLink.Save()
    foreach ($fixture in $desktopFixtures) { [LumaSmoke]::SHChangeNotify(2,0x1005,$fixture,[IntPtr]::Zero) }
    $desktopHash = (Get-FileHash -LiteralPath $desktopFile).Hash
    $linkHash = (Get-FileHash -LiteralPath $desktopShortcut).Hash
    Invoke-OleProbe 'collect' @($desktopFile, $desktopFolder, $desktopShortcut)
    Invoke-OleProbe 'link' @($desktopFile, $desktopFolder, $desktopShortcut)
    Start-Sleep -Milliseconds 250
    $collectedLayout = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $collectedPaths = @($collectedLayout.fences[0].paths | Where-Object { $desktopFixtures -contains $_ })
    if ($collectedPaths.Count -ne 3) { throw 'Desktop drop did not store all three items.' }
    foreach ($path in $desktopFixtures) { if (-not (Test-Path -LiteralPath $path)) { throw "Desktop original was moved: $path" } }
    $storedFile = @($collectedPaths | Where-Object { [IO.Path]::GetFileName($_) -eq [IO.Path]::GetFileName($desktopFile) })[0]
    $storedFolder = @($collectedPaths | Where-Object { [IO.Path]::GetFileName($_) -eq [IO.Path]::GetFileName($desktopFolder) })[0]
    $storedLink = @($collectedPaths | Where-Object { [IO.Path]::GetFileName($_) -eq [IO.Path]::GetFileName($desktopShortcut) })[0]
    if ((Get-FileHash -LiteralPath $storedFile).Hash -ne $desktopHash -or
        (Get-FileHash -LiteralPath $storedLink).Hash -ne $linkHash -or
        [IO.File]::ReadAllText((Join-Path $storedFolder 'nested.txt')) -ne 'nested content') {
        throw 'Desktop collection changed file, folder, or shortcut content.'
    }
    if ($shell.CreateShortcut($storedLink).TargetPath -ne $copyPath) { throw 'Collected shortcut target changed.' }
    # Remove collected items through the app's actual Delete handler. Recovery
    # appends the three items at the end; End/Delete returns each to the desktop.
    for ($item = 0; $item -lt 3; $item++) {
        [void][LumaSmoke]::PostMessage($fence, 0x0100, [IntPtr]0x23, [IntPtr]::Zero)
        [void][LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1004, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 300
        [LumaSmoke]::WaitIdle($launcher)
    }
    foreach ($path in $desktopFixtures) { if (-not (Test-Path -LiteralPath $path)) { throw "Desktop restore missing: $path" } }
    if ((Get-FileHash -LiteralPath $desktopFile).Hash -ne $desktopHash -or
        (Get-FileHash -LiteralPath $desktopShortcut).Hash -ne $linkHash) { throw 'Restore changed original data.' }
    # Copy-only sources stay references until the user explicitly collects
    # already pinned desktop items via the collection's command.
    Invoke-OleProbe 'copy' @($desktopFile)
    if (-not (Test-Path -LiteralPath $desktopFile)) { throw 'Copy-only desktop source was moved.' }
    [void][LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1017, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 400
    if (-not (Test-Path -LiteralPath $desktopFile)) { throw 'Updating desktop references moved the original.' }
    [void][LumaSmoke]::PostMessage($fence, 0x0100, [IntPtr]0x23, [IntPtr]::Zero)
    [void][LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1004, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 300
    if ((Get-FileHash -LiteralPath $desktopFile).Hash -ne $desktopHash) { throw 'Existing reference did not restore correctly.' }
    Write-Output 'VISUAL_GROUP_OK paths=unchanged file=preserved folder=preserved shortcut=preserved delete=reference_only existing_pins=reference'
    # Restore the original test HWND and create the second group using the same
    # native launcher command as its button. Never touch the user's app data.
    $fence = [LumaSmoke]::FirstFence()
    $outboundReference = Join-Path $temporary ($fixtureName + '-outbound.txt')
    [IO.File]::WriteAllText($outboundReference, 'Outbound shortcut source stays in place')
    $desktopFixtures += (Join-Path $desktopRoot ($fixtureName + '-outbound.txt.lnk'))
    [void][LumaSmoke]::SendFileDrop($fence, $outboundReference)
    Start-Sleep -Milliseconds 400
    [LumaSmoke]::WaitIdle($launcher)
    [void][LumaSmoke]::PostMessage($launcher, 0x0111, [IntPtr]1001, [IntPtr]::Zero)
    $targetFence = [IntPtr]::Zero
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        $targetFence = [LumaSmoke]::OtherFence([uint32]$proc.Id, $fence)
        if ($targetFence -ne [IntPtr]::Zero -and $targetFence -ne $fence) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($targetFence -eq [IntPtr]::Zero -or $targetFence -eq $fence) { throw 'Second group HWND unavailable.' }
    $arrangeProbe = Join-Path $project 'target\debug\examples\arrange_probe.exe'
    $arrangeStart = New-Object Diagnostics.ProcessStartInfo
    $arrangeStart.FileName = $arrangeProbe
    $arrangeStart.Arguments = '"' + $fence.ToInt64() + '" "' + $targetFence.ToInt64() + '" "' + (Join-Path $configDirectory 'layout.json') + '"'
    $arrangeStart.UseShellExecute = $false
    $arrangeStart.CreateNoWindow = $true
    $arrangeStart.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $arrangeStart.RedirectStandardOutput = $true
    $arrangeStart.RedirectStandardError = $true
    $arrangeProcess = New-Object Diagnostics.Process
    $arrangeProcess.StartInfo = $arrangeStart
    [void]$arrangeProcess.Start()
    if (-not $arrangeProcess.WaitForExit(15000)) { Stop-Process -Id $arrangeProcess.Id -Force; throw 'Arrangement probe timed out.' }
    $arrangeOutput = $arrangeProcess.StandardOutput.ReadToEnd()
    $arrangeError = $arrangeProcess.StandardError.ReadToEnd()
    if ($arrangeProcess.ExitCode -ne 0) { throw "Arrangement probe failed: $arrangeError $arrangeOutput" }
    $arrangeOutput.Trim()
    if (-not (Test-Path -LiteralPath $outboundReference)) { throw 'Outbound shortcut moved the original reference.' }

    $second = Start-Process -FilePath $exe -WindowStyle Hidden -PassThru
    if (-not $second.WaitForExit(2500)) {
        throw 'Single-instance guard did not reject the second launch.'
    }
    for ($attempt = 0; $attempt -lt 20 -and -not [LumaSmoke]::IsWindowVisible($launcher); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not [LumaSmoke]::IsWindowVisible($launcher) -or [LumaSmoke]::Launcher([uint32]$proc.Id) -ne $launcher) {
        throw 'Opening the app again did not restore the existing launcher.'
    }
    [void][LumaSmoke]::PostMessage($launcher, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
    for ($attempt = 0; $attempt -lt 30 -and [LumaSmoke]::IsWindowVisible($launcher); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    $background = Start-Process -FilePath $exe -ArgumentList '--background' -WindowStyle Hidden -PassThru
    if (-not $background.WaitForExit(2500) -or [LumaSmoke]::IsWindowVisible($launcher)) {
        throw 'A background re-launch must exit without showing the launcher.'
    }
    # Verify persistent undo across an actual process shutdown/restart.
    # Automatic organization scans the real desktop read-only, then explicitly
    # selects ONLY this run's GUID fixtures before invoking the apply button.
    $autoPrefix = $fixtureName + '-auto-'
    $autoFixtures = @()
    foreach ($extension in @('txt','png','mp4','zip','lnk','url','exe','ps1')) {
        $autoFile = Join-Path $desktopRoot ($autoPrefix + '中文.' + $extension)
        $autoFixtures += $autoFile
        $desktopFixtures += $autoFile
        if ($extension -eq 'lnk') { Copy-Item -LiteralPath $desktopShortcut -Destination $autoFile }
        elseif ($extension -eq 'url') { [IO.File]::WriteAllText($autoFile, "[InternetShortcut]`r`nURL=https://example.com/`r`n") }
        else { [IO.File]::WriteAllText($autoFile, 'Auto organization fixture ' + $extension) }
    }
    $autoFolder = Join-Path $desktopRoot ($autoPrefix + '中文資料夾')
    New-Item -ItemType Directory -Path (Join-Path $autoFolder 'nested') | Out-Null
    [IO.File]::WriteAllText((Join-Path $autoFolder 'nested\content.txt'), 'Nested automatic folder contents')
    $autoFixtures += $autoFolder
    $desktopFixtures += $autoFolder
    foreach ($fixture in $autoFixtures) { [LumaSmoke]::SHChangeNotify(2,0x1005,$fixture,[IntPtr]::Zero) }
    $autoFixtureCount = $autoFixtures.Count
    function Get-AutoFixtureHash([string]$Path) {
        $Path = $Path.Replace('\\?\','')
        if (Test-Path -LiteralPath $Path -PathType Container) { return (Get-FileHash -LiteralPath (Join-Path $Path 'nested\content.txt')).Hash }
        return (Get-FileHash -LiteralPath $Path).Hash
    }
    $autoHashes = @($autoFixtures | ForEach-Object { Get-AutoFixtureHash $_ })
    $settingsFile = Join-Path $configDirectory 'layout.json'
    $autoBefore = Get-Content -LiteralPath $settingsFile -Raw -Encoding UTF8 | ConvertFrom-Json
    $autoExpectedHistoryCount = [Math]::Min(20, $autoBefore.history.Count + 1)
    $autoBeforeHash = (Get-FileHash -LiteralPath $settingsFile).Hash
    for ($scan = 0; $scan -lt 2; $scan++) {
        [void][LumaSmoke]::PostMessage($launcher, 0x0111, [IntPtr]1022, [IntPtr]::Zero)
        $preview = [IntPtr]::Zero
        for ($attempt = 0; $attempt -lt 100; $attempt++) {
            $preview = [LumaSmoke]::WindowOfClass([uint32]$proc.Id, 'LumaDeskOrganizePreviewV1')
            if ($preview -ne [IntPtr]::Zero) { break }
            Start-Sleep -Milliseconds 100
        }
        if ($preview -eq [IntPtr]::Zero) { throw 'Automatic preview did not open.' }
        if ((Get-FileHash -LiteralPath $settingsFile).Hash -ne $autoBeforeHash) { throw 'Preview changed persistent data.' }
        foreach ($autoFile in $autoFixtures) { if (-not (Test-Path -LiteralPath $autoFile)) { throw 'Preview moved a file.' } }
        if ($scan -eq 0) {
            [void][LumaSmoke]::PostMessage($preview, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
            for ($attempt = 0; $attempt -lt 40; $attempt++) {
                if ([LumaSmoke]::WindowOfClass([uint32]$proc.Id, 'LumaDeskOrganizePreviewV1') -eq [IntPtr]::Zero) { break }
                Start-Sleep -Milliseconds 50
            }
        } else {
            # Synchronous selection must succeed before posting any apply command.
            [LumaSmoke]::SelectAutoFixtures($preview, $autoPrefix, $autoFixtureCount)
            $autoWatch = [Diagnostics.Stopwatch]::StartNew()
            [void][LumaSmoke]::PostMessage($preview, 0x0111, [IntPtr]1023, [IntPtr]::Zero)
        }
    }
    $autoDone = $false
    for ($attempt = 0; $attempt -lt 150; $attempt++) {
        $autoLayout = Get-Content -LiteralPath $settingsFile -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($null -eq $autoLayout.pending -and $autoLayout.history.Count -eq $autoExpectedHistoryCount -and
            $autoLayout.history[-1].id -ne $autoBefore.history[-1].id -and
            $autoLayout.history[-1].label -eq '自動分組桌面圖示' -and
            $autoLayout.history[-1].changes.Count -eq $autoFixtureCount) { $autoDone = $true; break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $autoDone) {
        throw "Automatic organization did not finish: history=$($autoLayout.history.Count), latest=$($autoLayout.history[-1].label), changes=$($autoLayout.history[-1].changes.Count), pending=$($null -ne $autoLayout.pending)"
    }
    $autoWatch.Stop()
    foreach ($autoFile in $autoFixtures) { if (-not (Test-Path -LiteralPath $autoFile)) { throw 'Automatic grouping moved its fixture.' } }
    $autoStored = @($autoLayout.fences | ForEach-Object { $_.entries } | Where-Object { [IO.Path]::GetFileName($_.path).StartsWith($autoPrefix) })
    if ($autoStored.Count -ne $autoFixtureCount) { throw 'Automatic collection stored unexpected items.' }
    if (@($autoStored | ForEach-Object { [IO.Path]::GetDirectoryName($_.path) } | Select-Object -Unique).Count -ne 1) { throw 'Automatic batch lost sibling relationships.' }
    for ($index = 0; $index -lt $autoFixtureCount; $index++) {
        $stored = @($autoStored | Where-Object { [IO.Path]::GetFileName($_.path) -eq [IO.Path]::GetFileName($autoFixtures[$index]) })
        if ($stored.Count -ne 1 -or $stored[0].kind -ne 'reference' -or $stored[0].path.Replace('\\?\','') -ne $autoFixtures[$index].Replace('\\?\','') -or (Get-AutoFixtureHash $stored[0].path) -ne $autoHashes[$index]) { throw 'Automatic grouping changed original path or content.' }
    }
    $iconProbe = Join-Path $project 'target/debug/examples/desktop_view_probe.exe'
    $iconJournal = $null
    for ($attempt = 0; $attempt -lt 80; $attempt++) {
        $iconJournal = @(Get-ChildItem -LiteralPath $configDirectory -Filter ("icon-session-" + $proc.Id + '-*.json')) | Select-Object -First 1
        if ($iconJournal) {
            $snapshot = Get-Content -LiteralPath $iconJournal.FullName -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($snapshot.icons.Count -eq $autoFixtureCount) { break }
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $iconJournal -or $snapshot.icons.Count -ne $autoFixtureCount) { throw 'Icon display journal missing fixture positions.' }
    $iconSnapshot = Join-Path $temporary 'icon-position-snapshot.json'
    Copy-Item -LiteralPath $iconJournal.FullName -Destination $iconSnapshot
    & $iconProbe hidden $iconSnapshot
    if ($LASTEXITCODE -ne 0) { throw 'Grouped Explorer icons remain visible.' }
    # Restart with the automatic batch as the newest operation; one Undo must
    # restore all eight categories and preserve retained history (20-entry cap).
    [void][LumaSmoke]::PostMessage($controller, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
    if (-not $proc.WaitForExit(5000)) { throw 'Automatic batch queue did not drain.' }
    & $iconProbe restored $iconSnapshot
    if ($LASTEXITCODE -ne 0) { throw 'Normal exit did not restore desktop icon positions.' }

    $env:APPDATA = $temporary
    try { $proc = Start-Process -FilePath $exe -WindowStyle Hidden -PassThru }
    finally { $env:APPDATA = $oldAppData }
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        $controller = [LumaSmoke]::Controller([uint32]$proc.Id)
        $launcher = [LumaSmoke]::Launcher([uint32]$proc.Id)
        $fence = [LumaSmoke]::FirstFence()
        if ($controller -ne [IntPtr]::Zero -and $launcher -ne [IntPtr]::Zero -and $fence -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 100
    }
    $crashMaskReady = $false
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        $crashJournal = @(Get-ChildItem -LiteralPath $configDirectory -Filter ("icon-session-" + $proc.Id + '-*.json')) | Select-Object -First 1
        if ($crashJournal -and (Get-Content -LiteralPath $crashJournal.FullName -Raw -Encoding UTF8 | ConvertFrom-Json).icons.Count -eq $autoFixtureCount) { $crashMaskReady = $true; break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $crashMaskReady) { throw 'Crash fixture was never concealed.' }
    & $iconProbe hidden $iconSnapshot
    if ($LASTEXITCODE -ne 0) { throw 'Crash fixture icons were visible before termination.' }
    # This PID belongs exclusively to the isolated smoke fixture, never the user's app.
    Stop-Process -Id $proc.Id -Force
    [void]$proc.WaitForExit(5000)
    for ($attempt = 0; $attempt -lt 80; $attempt++) {
        if (@(Get-ChildItem -LiteralPath $configDirectory -Filter 'icon-session-*.json').Count -eq 0) { break }
        Start-Sleep -Milliseconds 100
    }
    & $iconProbe restored $iconSnapshot
    if ($LASTEXITCODE -ne 0) { throw 'Watchdog did not restore desktop icon positions after UI crash.' }
    $env:APPDATA = $temporary
    try { $proc = Start-Process -FilePath $exe -WindowStyle Hidden -PassThru }
    finally { $env:APPDATA = $oldAppData }
    $env:LUMADESK_SMOKE_PID = [string]$proc.Id
    Start-Sleep -Milliseconds 1200
    $controller = [LumaSmoke]::WindowOfClass([uint32]$proc.Id, 'LumaDeskControllerV1')
    $launcher = [LumaSmoke]::WindowOfClass([uint32]$proc.Id, 'LumaDeskLauncherV1')
    if ($controller -eq [IntPtr]::Zero -or $launcher -eq [IntPtr]::Zero) { throw 'Watchdog restart failed.' }
    [void][LumaSmoke]::PostMessage($launcher, 0x0111, [IntPtr]1018, [IntPtr]::Zero)
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        $autoUndone = Get-Content -LiteralPath $settingsFile -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($null -eq $autoUndone.pending -and $autoUndone.history.Count -eq $autoExpectedHistoryCount - 1) { break }
        Start-Sleep -Milliseconds 100
    }
    $retainedHistory = @($autoBefore.history | Select-Object -Last ($autoExpectedHistoryCount - 1) | ForEach-Object { $_.id }) -join ','
    if ((@($autoUndone.history | ForEach-Object { $_.id }) -join ',') -ne $retainedHistory) { throw 'Undo changed retained history entries.' }
    for ($index = 0; $index -lt $autoFixtureCount; $index++) {
        if (-not (Test-Path -LiteralPath $autoFixtures[$index]) -or
            (Get-AutoFixtureHash $autoFixtures[$index]) -ne $autoHashes[$index]) { throw 'Restart Undo changed automatic fixture content.' }
    }
    Write-Output "AUTO_ORGANIZE_OK preview=read_only cancel=unchanged selection=fixtures_only categories=8 original_paths=unchanged nested_folder=preserved undo_after_restart=one_batch display_mask=verified normal_exit=restored crash_watchdog=restored elapsed_ms=$($autoWatch.ElapsedMilliseconds)"
    $fence = [LumaSmoke]::FindWindowEx([LumaSmoke]::DesktopView(), [IntPtr]::Zero, 'LumaDeskFenceV1', 'Smoke test')
    if ($fence -eq [IntPtr]::Zero) { throw 'Original test group was not reconciled after restart.' }
    [LumaSmoke]::WaitIdle($launcher)
    $beforeRestart = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $beforeRestartIds = @($beforeRestart.fences[0].entries | ForEach-Object { $_.id }) -join ','
    [void][LumaSmoke]::PostMessage($fence, 0x0100, [IntPtr]0x24, [IntPtr]::Zero)
    [void][LumaSmoke]::PostMessage($fence, 0x0111, [IntPtr]1004, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 400
    $deletedForRestart = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($deletedForRestart.fences[0].paths.Count -ne $beforeRestart.fences[0].paths.Count - 1) { throw 'Persistent undo setup failed.' }
    [void][LumaSmoke]::PostMessage($controller, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
    if (-not $proc.WaitForExit(5000)) { throw 'Queue did not drain on shutdown.' }
    $env:APPDATA = $temporary
    try { $proc = Start-Process -FilePath $exe -ArgumentList '--background' -WindowStyle Hidden -PassThru }
    finally { $env:APPDATA = $oldAppData }
    $controller = [IntPtr]::Zero
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        $controller = [LumaSmoke]::Controller([uint32]$proc.Id)
        $freshFence = [LumaSmoke]::FindWindowEx([LumaSmoke]::DesktopView(), [IntPtr]::Zero, 'LumaDeskFenceV1', 'Smoke test')
        if ($controller -ne [IntPtr]::Zero -and $freshFence -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($controller -eq [IntPtr]::Zero -or $freshFence -eq [IntPtr]::Zero) { throw 'Restart fixture windows unavailable.' }
    [void][LumaSmoke]::PostMessage($freshFence, 0x0111, [IntPtr]1018, [IntPtr]::Zero)
    for ($attempt=0; $attempt -lt 100; $attempt++) {
        $undoneAfterRestart = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        if ((@($undoneAfterRestart.fences[0].entries | ForEach-Object { $_.id }) -join ',') -eq $beforeRestartIds) { break }
        Start-Sleep -Milliseconds 100
    }
    if ((@($undoneAfterRestart.fences[0].entries | ForEach-Object { $_.id }) -join ',') -ne $beforeRestartIds) { throw 'First persistent undo did not restore the exact stable item order.' }
    [void][LumaSmoke]::PostMessage($freshFence, 0x0111, [IntPtr]1018, [IntPtr]::Zero)
    for ($attempt=0; $attempt -lt 100; $attempt++) {
        $twiceAfterRestart = Get-Content (Join-Path $configDirectory 'layout.json') -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($twiceAfterRestart.fences[0].paths.Count -eq $beforeRestart.fences[0].paths.Count - 1 -and $twiceAfterRestart.fences[0].paths -notcontains $outboundReference) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($twiceAfterRestart.fences[0].paths.Count -ne $beforeRestart.fences[0].paths.Count - 1 -or $twiceAfterRestart.fences[0].paths -contains $outboundReference) { throw 'Second persistent undo did not remove only the previous reference addition.' }
    if (-not (Test-Path -LiteralPath $outboundReference)) { throw 'Undo removed an external source file.' }
    Write-Output 'RESTART_UNDO_OK stable_ids=preserved sequential_undo=2 original_reference=preserved'
    Write-Output 'LAUNCHER_OK first_launch=visible close=background relaunch=restored background_launch=quiet'
    Write-Output "SMOKE_OK PID=$($proc.Id) RECT=$($rect.Left),$($rect.Top),$($rect.Right),$($rect.Bottom)"
    Write-Output "WORKING_SET_MB=$([Math]::Round($proc.WorkingSet64 / 1MB, 1))"
} finally {
    if ($env:LUMADESK_ICON_TRACE -and (Test-Path (Join-Path $configDirectory 'layout.json'))) { Copy-Item -LiteralPath (Join-Path $configDirectory 'layout.json') -Destination (Join-Path $project 'target/smoke-debug-layout.json') -Force }
    if ($env:LUMADESK_ICON_TRACE) { Get-ChildItem -LiteralPath $configDirectory -Filter '*.trace' | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $project ('target/' + $_.Name)) -Force } }
    $dropDiagnostics = Join-Path $configDirectory 'diagnostics.log'
    if (Test-Path -LiteralPath $dropDiagnostics) { Get-Content -LiteralPath $dropDiagnostics | Select-Object -Last 12 }
    $env:APPDATA = $oldAppData
    $env:LUMADESK_SMOKE_PID = $oldSmokePid
    if ($null -ne $proc -and -not $proc.HasExited) {
        $controller = [LumaSmoke]::Controller([uint32]$proc.Id)
        if ($controller -ne [IntPtr]::Zero) {
            [void][LumaSmoke]::PostMessage($controller, 0x10, [IntPtr]::Zero, [IntPtr]::Zero)
            [void]$proc.WaitForExit(3000)
        }
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
    }
    foreach ($fixture in $desktopFixtures) {
        $resolvedFixture = [IO.Path]::GetFullPath($fixture)
        if ([string]::Equals([IO.Path]::GetDirectoryName($resolvedFixture), $desktopRoot, [StringComparison]::OrdinalIgnoreCase) -and
            [IO.Path]::GetFileName($resolvedFixture).StartsWith($fixtureName) -and (Test-Path -LiteralPath $resolvedFixture)) {
            Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
        }
    }
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $actualTemporary = [IO.Path]::GetFullPath($temporary)
    if ($actualTemporary.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
        [IO.Path]::GetFileName($actualTemporary).StartsWith('lumadesk-smoke-')) {
        Remove-Item -LiteralPath $actualTemporary -Recurse -Force
    }
}
