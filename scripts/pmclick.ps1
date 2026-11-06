param([int]$x = 0, [int]$y = 0, [string]$mod = "", [string]$btn = "left", [int]$count = 1, [string]$key = "")
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class PM {
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint from, uint to, bool attach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref P p);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    public struct R { public int l, t, r, b; }
    public struct P { public int x, y; }
}
'@
$p = Get-Process -Name 'pr-desk-rs' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { Write-Output "no pr-desk-rs process"; exit 1 }
$h = $p.MainWindowHandle
$pt = New-Object PM+P; [PM]::ClientToScreen($h, [ref]$pt) | Out-Null
$scale = [PM]::GetDpiForWindow($h) / 96.0
$fg = [PM]::GetForegroundWindow()
$fgTid = [PM]::GetWindowThreadProcessId($fg, [IntPtr]::Zero)
$myTid = [PM]::GetCurrentThreadId()
[PM]::AttachThreadInput($myTid, $fgTid, $true) | Out-Null
[PM]::SetForegroundWindow($h) | Out-Null
[PM]::BringWindowToTop($h) | Out-Null
[PM]::AttachThreadInput($myTid, $fgTid, $false) | Out-Null
Start-Sleep -Milliseconds 150

if ($key -ne "") {
    $vkMap = @{ 'F12'=0x7B; 'J'=0x4A; 'K'=0x4B; 'ENTER'=0x0D; 'ESC'=0x1B; 'O'=0x4F; 'P'=0x50; '1'=0x31; '2'=0x32; '3'=0x33; 'SLASH'=0xBF; '0'=0x30 }
    $vk = $vkMap[$key.ToUpper()]
    [PM]::PostMessageW($h, 0x0100, [IntPtr]$vk, [IntPtr]0) | Out-Null  # WM_KEYDOWN
    Start-Sleep -Milliseconds 40
    [PM]::PostMessageW($h, 0x0101, [IntPtr]$vk, [IntPtr]0) | Out-Null  # WM_KEYUP
    Write-Output "posted key $key"
    exit 0
}

# 点击：logical(client) 坐标 → physical（DPI 缩放）；lParam 用 client px（winit 读 lParam）
[PM]::SetCursorPos([int]($pt.x + $x * $scale), [int]($pt.y + $y * $scale)) | Out-Null
Start-Sleep -Milliseconds 200
$mk = [IntPtr]0
if ($mod -eq 'ctrl') { $mk = [IntPtr]8 } elseif ($mod -eq 'shift') { $mk = [IntPtr]4 }
$px = [int]($x * $scale); $py = [int]($y * $scale)
$lpVal = ([int64]($py) -band 0xFFFF) -shl 16 -bor ([int64]($px) -band 0xFFFF)
$lp = [IntPtr]$lpVal
$mkBtn = [IntPtr]([int64]$mk -bor 1)
for ($i = 0; $i -lt $count; $i++) {
    # 先 MOUSEMOVE（winit 的按键事件不带位置，egui 用最近的光标位置）
    $mv = [PM]::PostMessageW($h, 0x0200, [IntPtr]0, $lp)   # WM_MOUSEMOVE
    Start-Sleep -Milliseconds 60
    if ($btn -eq 'right') {
        $a = [PM]::PostMessageW($h, 0x0204, $mk, $lp)  # WM_RBUTTONDOWN
        Start-Sleep -Milliseconds 40
        $b = [PM]::PostMessageW($h, 0x0205, $mk, $lp)  # WM_RBUTTONUP
    } else {
        $a = [PM]::PostMessageW($h, 0x0201, $mkBtn, $lp)  # WM_LBUTTONDOWN
        Start-Sleep -Milliseconds 40
        $b = [PM]::PostMessageW($h, 0x0202, $mk, $lp)  # WM_LBUTTONUP
    }
    Write-Output "  post results: move=$mv down=$a up=$b"
    Start-Sleep -Milliseconds 120
}
Write-Output "posted $btn click x$count at client+$x,$y mod=$mod (scale=$scale)"
