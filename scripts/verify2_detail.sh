#!/bin/bash
# 验证 2：真实键盘路径（PostMessage WM_KEYDOWN）+ 详情面板 tab 惰性加载
source /d/Code/pr-desk-rs-egui/scripts/verify.sh "键盘+详情面板"
# 前置：确保窗口可见（桌面其它活动可能把窗口最小化/盖住——隐藏窗口不绘制、键鼠语义不同）
: > "$CMD"
echo "trayshow" > "$CMD"
sleep 1

# 真键盘：J/K 移动（PostMessage WM_KEYDOWN → winit → egui → hotkeys）
pmkey() { powershell -ExecutionPolicy Bypass -File /d/Code/pr-desk-rs-egui/scripts/pmclick.ps1 -key "$1" >/dev/null; sleep 1.2; }
# 轮询等待状态条件（powershell 启动 ~2s + 处理延迟，固定 sleep 不可靠）
wait_state() {  # wait_state <python-expr>
  for i in $(seq 1 25); do
    local v
    v=$(state | python -c "import json,sys; d=json.load(sys.stdin); print(1 if ($1) else 0)" 2>/dev/null)
    [ "$v" = "1" ] && return 0
    sleep 0.4
  done
  return 1
}
pmkey J
wait_state "d['selected'] is not None" || true
assert "真键盘 j → 选中行" "d['selected'] is not None"
S1=$(state | python -c "import json,sys; print(json.load(sys.stdin)['selected'][1])")
pmkey J
wait_state "d['selected'] is not None and d['selected'][1] != $S1" || true
S2=$(state | python -c "import json,sys; print(json.load(sys.stdin)['selected'][1])")
[ "$S1" != "$S2" ] && echo "  PASS: 真键盘 j 移动（$S1 → $S2）" && PASS=$((PASS+1)) || { echo "  FAIL: 真键盘 j 未移动"; FAIL=$((FAIL+1)); }

# Enter 开详情（真键盘）
pmkey ENTER
wait_state "d['panel_open']==True" || true
assert "Enter → 详情面板打开" "d['panel_open']==True"
assert "面板身份 = 选中行" "d['panel_number']==d['selected'][1]"
shot "detail-review-tab"

# tab 惰性加载：会话
cmd "tab 1"; sleep 2.5
assert "会话 tab：拉到会话内容（convo>0 或保持空数组但无错）" "d['panel_tab']==1"
CONVO=$(state | python -c "import json,sys; print(json.load(sys.stdin)['panel_convo'])")
echo "  [convo 条数] $CONVO"
shot "detail-convo-tab"

# Checks
cmd "tab 2"; sleep 2.5
assert "Checks tab 打开" "d['panel_tab']==2"
shot "detail-checks-tab"

# Files
cmd "tab 3"; sleep 2.5
assert "Files tab 打开" "d['panel_tab']==3"
shot "detail-files-tab"

# Diff（虚拟化）
cmd "tab 4"; sleep 1
assert "Diff tab：已解析文件数 >0" "d['diff_files']>0"
shot "detail-diff-tab"

# 复制评审（无评审记录 → 不应崩；剪贴板不变即正确跳过）
cmd "copyreview"
assert "copyreview 无评审时不崩" "d['panel_open']==True"

# 评审触发（LLM 未配置 → llm_not_configured 状态提示，不占槽不崩）
cmd "review"; sleep 1
assert "评审触发（LLM 未配置→状态提示，面板不崩）" "d['panel_open']==True"
SM=$(state | python -c "import json,sys; print(json.load(sys.stdin)['status_msg'])")
echo "  [status] $SM"
echo "$SM" | grep -qE "LLM|未配置" && { echo "  PASS: llm_not_configured 提示"; PASS=$((PASS+1)); } || { echo "  FAIL: 无 LLM 提示: $SM"; FAIL=$((FAIL+1)); }

# Esc 关面板
pmkey ESC
assert "Esc → 关面板+清选中" "d['panel_open']==False and d['selected'] is None"

echo "=== 结果: $PASS pass / $FAIL fail ==="
[ $FAIL -eq 0 ]
