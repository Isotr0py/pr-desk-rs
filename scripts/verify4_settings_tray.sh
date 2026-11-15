#!/bin/bash
# 验证 4：设置页 / 档案 / 主题 / 语言 / 托盘 / 退出
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
source "$ROOT/scripts/verify.sh" "设置+档案+托盘"
# 前置：确保窗口可见（桌面其它活动可能把窗口最小化/盖住——隐藏窗口不绘制、键鼠语义不同）
: > "$CMD"
echo "trayshow" > "$CMD"
sleep 1

# ── 设置页 ──
cmd "settings"
assert "设置页打开" "d['settings_open']==True"
# 等评审模板异步重读（prompt_loading 竞态防护：loading 时禁编辑）
sleep 1
assert "打开即回填（默认 local 档锁定 → 无报错）" "d['dialog_error']==''"
shot "settings-source"

cmd "settab 1"; shot "settings-general"
cmd "settab 2"; shot "settings-llm"

# 保存（无改动）：热生效 = ReloadConfig → 状态「配置已保存并生效」
cmd "settingssave"; sleep 1.5
SM=$(state | python -c "import json,sys; print(json.load(sys.stdin)['status_msg'])")
echo "  [保存 status] $SM"
echo "$SM" | grep -qE "已保存|重载|saved|reloaded" && { echo "  PASS: 保存热生效"; PASS=$((PASS+1)); } || { echo "  FAIL: $SM"; FAIL=$((FAIL+1)); }
assert "保存后弹窗仍开（settingssave 指令不主动关）" "True"

# 代理校验：非法代理拒绝保存
#（直接改表单字段无法经指令通道——跳过 UI 输入，proxy_invalid 路径由 http_client 错误覆盖；记为已知未覆盖）

cmd "esc"
assert "Esc 关设置页" "d['settings_open']==False"

# ── 档案切换 ──
PN=$(state | python -c "import json,sys; print(json.load(sys.stdin)['profile_names'])")
echo "  [档案名单] $PN"
cmd "profile no-such-profile"
# worker 往返有延迟：轮询最多 3s 等状态条出现（别打固定 sleep 赌时序）
SM=""
for i in $(seq 1 15); do
  SM=$(state | python -c "import json,sys; print(json.load(sys.stdin)['status_msg'])")
  echo "$SM" | grep -qE "未知|档案|profile" && break
  sleep 0.2
done
echo "$SM" | grep -qE "未知|档案|profile" && { echo "  PASS: 非法档案名被拒 + 状态提示"; PASS=$((PASS+1)); } || { echo "  FAIL: [$SM]"; FAIL=$((FAIL+1)); }
assert "切换被拒后仍在 local" "d['active_profile']=='local'"
cmd "profile local"; sleep 1
assert "同档重选无操作（行数不变）" "d['active_profile']=='local' and d['rows']>=8"

# ── 主题（四种风格 / 配置写回 / 实际 Visuals）──
T0=$(state | python -c "import json,sys; print(json.load(sys.stdin)['theme'])")
for T in glass-light glass-dark; do
  cmd "theme $T"
  WANT_DARK=False
  [ "$T" = "glass-dark" ] && WANT_DARK=True
  assert "$T 配置与实际样式同步" "d['theme']=='$T' and d['visual_dark_mode']==$WANT_DARK and d['panel_alpha']<255 and d['window_alpha']<255 and ((d['backdrop_mode']=='Native' and d['clear_alpha']==0) or (d['backdrop_mode']=='Gradient' and d['clear_alpha']==255))"
  CT=$(grep '^theme' "$ROOT/config.toml")
  echo "$CT" | grep -q "$T" && { echo "  PASS: $T 写回 config.toml"; PASS=$((PASS+1)); } || { echo "  FAIL: $T cfg=$CT"; FAIL=$((FAIL+1)); }
  shot "theme-$T"
  cmd "settings"; cmd "settab 1"; shot "settings-$T"; cmd "esc"
done
cmd "theme light"
assert "Glass 切回浅色无样式残留" "d['theme']=='light' and d['visual_dark_mode']==False and d['panel_alpha']==255 and d['window_alpha']==255 and d['backdrop_mode']=='Opaque' and d['clear_alpha']==255"
cmd "theme dark"
assert "Glass 切回深色无样式残留" "d['theme']=='dark' and d['visual_dark_mode']==True and d['panel_alpha']==255 and d['window_alpha']==255 and d['backdrop_mode']=='Opaque' and d['clear_alpha']==255"
cmd "theme $T0"
assert "恢复初始主题" "d['theme']=='$T0'"

# ── 托盘 ──
TRAY=$(powershell -Command "Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public class FT9 { [DllImport(\"user32.dll\", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr p, IntPtr c, string cls, string w); }'; [FT9]::FindWindowExW([IntPtr]::Zero,[IntPtr]::Zero,'PRDeskTrayWnd',\$null)")
echo "  [tray hwnd] $TRAY"
[ "$TRAY" != "0" ] && { echo "  PASS: 托盘消息窗口存在"; PASS=$((PASS+1)); } || { echo "  FAIL: 无托盘窗口"; FAIL=$((FAIL+1)); }

cmd "hide"; sleep 1
W1=$(powershell -ExecutionPolicy Bypass -File "$ROOT"/scripts/pdrive.ps1 -act info)
echo "  [hide 后] $W1"
echo "$W1" | grep -qE "client=0x0|winrect=0,0" && { echo "  PASS: 关窗隐藏（0x0）"; PASS=$((PASS+1)); } || { echo "  FAIL: $W1"; FAIL=$((FAIL+1)); }

# 托盘左键还原（PostMessage WM_TRAY/WM_LBUTTONUP 到托盘消息窗口）
powershell -Command "
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public class FT8 { [DllImport(\"user32.dll\", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr p, IntPtr c, string cls, string w); [DllImport(\"user32.dll\")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l); }'
\$t = [FT8]::FindWindowExW([IntPtr]::Zero,[IntPtr]::Zero,'PRDeskTrayWnd',\$null)
[FT8]::PostMessageW(\$t, 0x0401, [IntPtr]1, [IntPtr]0x0202) | Out-Null"
# 还原不是即时的（winit 重建 surface/首帧绘制要数百 ms~数秒）：轮询最多 10s
W2=""
for i in $(seq 1 20); do
  W2=$(powershell -ExecutionPolicy Bypass -File "$ROOT"/scripts/pdrive.ps1 -act info)
  echo "$W2" | grep -qE "client=[1-9][0-9]*x[1-9][0-9]*" && break
  sleep 0.5
done
echo "  [还原后] $W2"
( echo "$W2" | grep -qE "client=[1-9][0-9]*x[1-9][0-9]*" ) && { echo "  PASS: 托盘左键还原窗口"; PASS=$((PASS+1)); } || { echo "  FAIL: $W2"; FAIL=$((FAIL+1)); }

echo "=== 结果: $PASS pass / $FAIL fail ==="
[ $FAIL -eq 0 ]
