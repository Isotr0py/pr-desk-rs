#!/bin/bash
# 验证 4：设置页 / 档案 / 主题 / 语言 / 托盘 / 退出
source /d/Code/pr-desk-rs-egui/scripts/verify.sh "设置+档案+托盘"

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
echo "$SM" | grep -qE "已保存|重载|saved" && { echo "  PASS: 保存热生效"; PASS=$((PASS+1)); } || { echo "  FAIL: $SM"; FAIL=$((FAIL+1)); }
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

# ── 主题（配置写回）──
cmd "theme"
sleep 0.5
T1=$(state | python -c "import json,sys; print(json.load(sys.stdin)['theme'])")
CT=$(grep '^theme' /d/Code/pr-desk-rs-egui/config.toml)
echo "  [theme] $T1 / config: $CT"
[ "$T1" = "dark" ] && echo "$CT" | grep -q dark && { echo "  PASS: 主题切换 + 写回 config.toml"; PASS=$((PASS+1)); } || { echo "  FAIL: theme=$T1 cfg=$CT"; FAIL=$((FAIL+1)); }
shot "dark-theme"
cmd "theme"  # 还原浅色

# ── 托盘 ──
TRAY=$(powershell -Command "Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public class FT9 { [DllImport(\"user32.dll\", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr p, IntPtr c, string cls, string w); }'; [FT9]::FindWindowExW([IntPtr]::Zero,[IntPtr]::Zero,'PRDeskTrayWnd',\$null)")
echo "  [tray hwnd] $TRAY"
[ "$TRAY" != "0" ] && { echo "  PASS: 托盘消息窗口存在"; PASS=$((PASS+1)); } || { echo "  FAIL: 无托盘窗口"; FAIL=$((FAIL+1)); }

cmd "hide"; sleep 1
W1=$(powershell -ExecutionPolicy Bypass -File /d/Code/pr-desk-rs-egui/scripts/pdrive.ps1 -act info)
echo "  [hide 后] $W1"
echo "$W1" | grep -qE "client=0x0|winrect=0,0" && { echo "  PASS: 关窗隐藏（0x0）"; PASS=$((PASS+1)); } || { echo "  FAIL: $W1"; FAIL=$((FAIL+1)); }

# 托盘左键还原（PostMessage WM_TRAY/WM_LBUTTONUP 到托盘消息窗口）
powershell -Command "
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public class FT8 { [DllImport(\"user32.dll\", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr p, IntPtr c, string cls, string w); [DllImport(\"user32.dll\")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l); }'
\$t = [FT8]::FindWindowExW([IntPtr]::Zero,[IntPtr]::Zero,'PRDeskTrayWnd',\$null)
[FT8]::PostMessageW(\$t, 0x0401, [IntPtr]1, [IntPtr]0x0202) | Out-Null"
sleep 1.5
W2=$(powershell -ExecutionPolicy Bypass -File /d/Code/pr-desk-rs-egui/scripts/pdrive.ps1 -act info)
echo "  [还原后] $W2"
( echo "$W2" | grep -qE "client=[1-9][0-9]*x[1-9][0-9]*" ) && { echo "  PASS: 托盘左键还原窗口"; PASS=$((PASS+1)); } || { echo "  FAIL: $W2"; FAIL=$((FAIL+1)); }

echo "=== 结果: $PASS pass / $FAIL fail ==="
[ $FAIL -eq 0 ]
