#!/bin/bash
# 验证 4：Deep Review 本地工作区（需 PR_DESK_TEST=1、可用 LLM 或本地假 LLM）
source "$(dirname "$0")/verify.sh" "Deep Review 只读工作区"

REPO="${PD_REPO:-octocat/Hello-World}"
NUMBER="${PD_NUMBER:-1}"

wait_state() {
  for _ in $(seq 1 40); do
    local v
    v=$(state | python -c "import json,sys; d=json.load(sys.stdin); print(1 if ($1) else 0)" 2>/dev/null)
    [ "$v" = "1" ] && return 0
    sleep 0.25
  done
  return 1
}

pmkey() {
  powershell -ExecutionPolicy Bypass -File /d/Code/pr-desk-rs/scripts/pmclick.ps1 -key "$1" >/dev/null
  sleep 1.2
}

: > "$CMD"
echo "trayshow" > "$CMD"
sleep 1
cmd "openpr $REPO $NUMBER"
wait_state "d['panel_open']==True" || true
cmd "deepseed"
wait_state "d.get('deep_findings',0)>0" || true
assert "本地 Finding 夹具已加载" "d.get('deep_findings',0)>0"
cmd "tab 4"
assert "Deep Review tab 打开" "d['panel_tab']==4"
ID=$(state | python -c "import json,sys; d=json.load(sys.stdin); print(d['deep_editors'][0]['id'])")

cmd "deepdraft $ID Edited local review comment"
cmd "deepsave $ID"
wait_state "d['deep_editors'][0]['revision']>=1 and d['deep_editors'][0]['pending'] is None" || true
assert "编辑并保存本地草稿" "d['deep_editors'][0]['draft']=='Edited local review comment' and d['deep_editors'][0]['revision']>=1"

cmd "deeprewrite $ID shorter"
wait_state "d['deep_editors'][0]['pending'] is None" || true
assert "LLM 改写完成且解除 pending" "d['deep_editors'][0]['pending'] is None and d['deep_editors'][0]['error']==''"
assert "改写后草稿已持久化" "d['deep_editors'][0]['revision']>=2"

cmd "deepdiscard $ID"
wait_state "d.get('deep_active')==0" || true
assert "丢弃建议评论" "d.get('deep_active')==0"
cmd "deepdiscard $ID"
wait_state "d.get('deep_active')==1" || true
assert "恢复建议评论" "d.get('deep_active')==1"

cmd "deepquestion $ID Is this a false positive?"
cmd "deepask $ID"
wait_state "d.get('deep_qa',0)>=2 and d['deep_editors'][0]['pending'] is None" || true
assert "Finding 级 Agent 问答" "d.get('deep_qa',0)>=2 and d['deep_editors'][0]['error']==''"
cmd "deepcopy $ID"
cmd "deepcopyall"
shot "deep-review-workspace"

cmd "enter"
wait_state "d.get('deep_findings',0)>0" || true
assert "重开面板恢复草稿与问答" "d['deep_editors'][0]['revision']>=3 and d.get('deep_qa',0)>=2"
cmd "deepstale"
wait_state "d.get('deep_head')!=d.get('deep_current_head')" || true
assert "旧提交工作区明确标识" "d.get('deep_head')!=d.get('deep_current_head')"

cmd "deepclearfindings"
assert "无 Findings 也保留 PR 级 Agent 入口" "d.get('deep_findings')==0 and d.get('deep_pr_qa') is not None"
cmd "deepprquestion What are the main risks of this PR?"
cmd "deepprfocus"
pmkey ENTER
wait_state "d.get('deep_pr_qa',0)>=2 and d['deep_pr_pending'] is None" || true
assert "整个 PR 级 Agent 问答" "d.get('deep_pr_qa',0)>=2 and d['deep_pr_error']==''"
cmd "enter"
wait_state "d.get('deep_pr_qa',0)>=2" || true
assert "PR 问答历史重开恢复" "d.get('deep_pr_qa',0)>=2 and d.get('deep_diff_chars',0)>0"
shot "deep-review-pr-agent"

cmd "deeprefreshfixture"
wait_state "d.get('deep_resolved',0)==1 and d.get('deep_findings',0)==2" || true
assert "刷新后旧 Finding 标为未再发现" "d.get('deep_resolved',0)==1 and any(x['resolution']=='not_found' for x in d['deep_resolutions'])"
assert "刷新保留旧草稿和问答并加入新 Finding" "d.get('deep_qa',0)>=2 and d.get('deep_active')==1 and d.get('deep_findings')==2"
shot "deep-review-refreshed"

cmd "deeprunning"; sleep 1.2
assert "运行中显示 OCR 实时阶段与耗时" "d['deep_status']=='running' and 'OCR' in d['deep_progress'] and d['deep_elapsed_secs']>=1"
shot "deep-review-running"
cmd "deepcancel"
assert "取消入口进入 cancelling 状态" "d['deep_status']=='cancelling'"
cmd "deepcancelresult"
assert "取消完成恢复按钮并清理运行态" "d['deep_status']=='failed' and d['deep_run'] is None and d['deep_elapsed_secs'] is None"

echo "=== 结果: $PASS pass / $FAIL fail ==="
[ $FAIL -eq 0 ]
