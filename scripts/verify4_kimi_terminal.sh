#!/bin/bash
# 验证 4：无 Deep/Findings 依赖的持久 Git 工作区与真实内嵌 Kimi CLI
source "$(dirname "$0")/verify.sh" "Kimi CLI 持久工作区"

REPO="${PD_REPO:-octocat/Hello-World}"
NUMBER="${PD_NUMBER:-1}"

wait_state() {
  for _ in $(seq 1 120); do
    local v
    v=$(state | python -c "import json,sys; d=json.load(sys.stdin); print(1 if ($1) else 0)" 2>/dev/null)
    [ "$v" = "1" ] && return 0
    sleep 0.5
  done
  return 1
}

: > "$CMD"
echo "trayshow" > "$CMD"
sleep 1
cmd "openpr $REPO $NUMBER"
wait_state "d['panel_open']==True and d.get('panel_number')==$NUMBER" || true
assert "无需 Deep 工作区即可打开 PR" "d['panel_open']==True and d.get('panel_number')==$NUMBER"

cmd "tab 4"
wait_state "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False and d.get('kimi_terminal_dimensions') is not None" || true
assert "Kimi 页签直接启动真实 CLI" "d['panel_tab']==4 and d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False and d.get('kimi_terminal_error')==''"
assert "ConPTY 已完成终端尺寸协商" "d.get('kimi_terminal_dimensions') is not None"
REQUEST_ID=$(state | python -c "import json,sys; print(json.load(sys.stdin).get('kimi_request_id') or 0)")
sleep 2
assert "绘制帧不会反复重启 CLI" "$REQUEST_ID > 0 and d.get('kimi_request_id')==$REQUEST_ID and d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False"

WORKSPACE=$(state | python -c "import json,sys; print(json.load(sys.stdin).get('kimi_workspace') or '')")
CONTEXT=$(dirname "$WORKSPACE")
assert "工作区路径已持久化到可配置根目录" "bool(d.get('kimi_workspace'))"
[ -n "$WORKSPACE" ] && [ -d "$WORKSPACE/.git" ] && { echo "  PASS: 完整 Git 工作树"; PASS=$((PASS+1)); } || { echo "  FAIL: 完整 Git 工作树: $WORKSPACE"; FAIL=$((FAIL+1)); }
[ -f "$CONTEXT/pr.json" ] && [ -f "$CONTEXT/pr-body.md" ] && [ -f "$CONTEXT/pr.diff" ] && { echo "  PASS: PR 描述、元数据与完整 diff"; PASS=$((PASS+1)); } || { echo "  FAIL: PR 上下文文件"; FAIL=$((FAIL+1)); }
[ ! -e "$CONTEXT/findings.json" ] && { echo "  PASS: 不再生成 findings.json"; PASS=$((PASS+1)); } || { echo "  FAIL: 仍有 findings.json"; FAIL=$((FAIL+1)); }
BASE_SHA=$(python -c "import json; print(json.load(open('$CONTEXT/pr.json'))['base_sha'])" 2>/dev/null)
if [ -n "$WORKSPACE" ] && [ -n "$BASE_SHA" ] && git -C "$WORKSPACE" cat-file -e "$BASE_SHA^{commit}" 2>/dev/null && git -C "$WORKSPACE" merge-base "$BASE_SHA" HEAD >/dev/null 2>&1; then
  echo "  PASS: Git 工作树包含 base/head 与 merge-base 历史"
  PASS=$((PASS+1))
else
  echo "  FAIL: Git 工作树历史不可用"
  FAIL=$((FAIL+1))
fi

cmd "kimiinput /help"
cmd "kimienter"
sleep 2
assert "真实 CLI 可接收斜杠命令且保持运行" "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_closed')==False and d.get('kimi_terminal_error')==''"
shot "kimi-cli-terminal"

cmd "kimirestart"
wait_state "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False" || true
assert "重启 CLI 并恢复最近会话" "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False and d.get('kimi_terminal_error')==''"
shot "kimi-cli-resumed"

cmd "kimikey esc"
cmd "kimiinput /new"
cmd "kimienter"
wait_state "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False" || true
assert "通过 CLI 原生命令新建会话" "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False and d.get('kimi_terminal_error')==''"
shot "kimi-cli-new-session"

cmd "review"
sleep 1
assert "普通预评审入口仍然可用" "d['panel_open']==True and (d.get('panel_review_status') in ('running','done','failed') or 'LLM' in d.get('status_msg','') or '未配置' in d.get('status_msg','') or '拉取' in d.get('status_msg','') or 'Fetching' in d.get('status_msg',''))"

echo "=== 结果: $PASS pass / $FAIL fail ==="
[ $FAIL -eq 0 ]
