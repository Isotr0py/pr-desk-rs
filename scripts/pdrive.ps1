param([string]$act, [int]$x = 0, [int]$y = 0, [string]$out = "", [string]$mod = "")
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class PD {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out R r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref P p);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint from, uint to, bool attach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    public struct R { public int l, t, r, b; }
    public struct P { public int x, y; }
}
'@
$p = Get-Process -Name 'pr-desk-rs' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $p) { Write-Output "no pr-desk-rs process"; exit 1 }
$h = $p.MainWindowHandle
$wr = New-Object PD+R; [PD]::GetWindowRect($h, [ref]$wr) | Out-Null
$cr = New-Object PD+R; [PD]::GetClientRect($h, [ref]$cr) | Out-Null
$pt = New-Object PD+P; [PD]::ClientToScreen($h, [ref]$pt) | Out-Null
$script:scale = [PD]::GetDpiForWindow($h) / 96.0
function Click-At($x, $y, $right, $mod) {
    $fg = [PD]::GetForegroundWindow()
    $fgTid = [PD]::GetWindowThreadProcessId($fg, [IntPtr]::Zero)
    $myTid = [PD]::GetCurrentThreadId()
    [PD]::AttachThreadInput($myTid, $fgTid, $true) | Out-Null
    [PD]::SetForegroundWindow($h) | Out-Null
    [PD]::BringWindowToTop($h) | Out-Null
    [PD]::AttachThreadInput($myTid, $fgTid, $false) | Out-Null
    Start-Sleep -Milliseconds 150
    # 修饰键按下（VK_CONTROL=0x11, VK_SHIFT=0x10；KEYEVENTF_KEYUP=2）
    $vk = 0
    if ($mod -eq 'ctrl') { $vk = 0x11 } elseif ($mod -eq 'shift') { $vk = 0x10 }
    if ($vk) { [PD]::keybd_event($vk, 0, 0, [UIntPtr]::Zero) }
    Start-Sleep -Milliseconds 100
    [PD]::SetCursorPos([int]($pt.x + $x * $script:scale), [int]($pt.y + $y * $script:scale)) | Out-Null
    Start-Sleep -Milliseconds 200
    if ($right) {
        [PD]::mouse_event(8, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 60
        [PD]::mouse_event(16, 0, 0, 0, [UIntPtr]::Zero)
    } else {
        [PD]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 60
        [PD]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
    }
    Start-Sleep -Milliseconds 80
    if ($vk) { [PD]::keybd_event($vk, 0, 2, [UIntPtr]::Zero) }
    Write-Output "clicked client+$x,$y mod=$mod right=$right"
}
switch ($act) {
    'info' { Write-Output ("pid={0} winrect={1},{2}-{3},{4} client0={5},{6} client={7}x{8}" -f $p.Id, $wr.l, $wr.t, $wr.r, $wr.b, $pt.x, $pt.y, $cr.r, $cr.b) }
    'grab' {
        $b = New-Object System.Drawing.Bitmap ($wr.r - $wr.l), ($wr.b - $wr.t)
        $g = [System.Drawing.Graphics]::FromImage($b)
        $g.CopyFromScreen($wr.l, $wr.t, 0, 0, $b.Size)
        $b.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
        $g.Dispose(); $b.Dispose()
        Write-Output "saved $out ($($wr.r - $wr.l)x$($wr.b - $wr.t))"
    }
    'click'  { Click-At $x $y $false $mod }
    'rclick' { Click-At $x $y $true $mod }
}
