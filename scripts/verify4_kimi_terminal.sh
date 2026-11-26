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

rm -f "$STATE" "$SHOT"
: > "$CMD"
echo "dumpstate" > "$CMD"
for _ in $(seq 1 120); do
  [ -f "$STATE" ] && break
  sleep 0.5
done
[ -f "$STATE" ] || { echo "测试实例未就绪"; exit 1; }
cmd "trayshow"
cmd "esc"
cmd "openpr $REPO $NUMBER"
wait_state "d['panel_open']==True and d.get('panel_number')==$NUMBER" || true
assert "无需 Deep 工作区即可打开 PR" "d['panel_open']==True and d.get('panel_number')==$NUMBER"

cmd "tab 4"
shot "kimi-cli-terminal"
wait_state "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False and d.get('kimi_terminal_dimensions') is not None" || true
assert "Kimi 页签直接启动真实 CLI" "d['panel_tab']==4 and d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False and d.get('kimi_terminal_error')==''"
assert "ConPTY 已完成终端尺寸协商" "d.get('kimi_terminal_dimensions') is not None"
REQUEST_ID=$(state | python -c "import json,sys; print(json.load(sys.stdin).get('kimi_request_id') or 0)")
sleep 2
assert "绘制帧不会反复重启 CLI" "$REQUEST_ID > 0 and d.get('kimi_request_id')==$REQUEST_ID and d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False"

WORKSPACE=$(state | python -c "import json,sys; print((json.load(sys.stdin).get('kimi_workspace') or '').replace(chr(92), '/'))")
CONTEXT=$(dirname "$WORKSPACE")
assert "工作区路径已持久化到可配置根目录" "bool(d.get('kimi_workspace'))"
[ -n "$WORKSPACE" ] && [ -d "$WORKSPACE/.git" ] && { echo "  PASS: 完整 Git 工作树"; PASS=$((PASS+1)); } || { echo "  FAIL: 完整 Git 工作树: $WORKSPACE"; FAIL=$((FAIL+1)); }
[ -f "$WORKSPACE/.git/objects/info/alternates" ] && grep -q '.git-mirrors' "$WORKSPACE/.git/objects/info/alternates" && { echo "  PASS: 工作树通过 alternates 共享 mirror 对象"; PASS=$((PASS+1)); } || { echo "  FAIL: 工作树未使用共享 mirror"; FAIL=$((FAIL+1)); }
[ -f "$CONTEXT/pr.json" ] && [ -f "$CONTEXT/pr-body.md" ] && [ -f "$CONTEXT/pr.diff" ] && { echo "  PASS: PR 描述、元数据与完整 diff"; PASS=$((PASS+1)); } || { echo "  FAIL: PR 上下文文件"; FAIL=$((FAIL+1)); }
[ ! -e "$CONTEXT/findings.json" ] && { echo "  PASS: 不再生成 findings.json"; PASS=$((PASS+1)); } || { echo "  FAIL: 仍有 findings.json"; FAIL=$((FAIL+1)); }
BASE_SHA=$(python -c "import json; print(json.load(open('$CONTEXT/pr.json'))['base_sha'])" 2>/dev/null)
if [ -n "$WORKSPACE" ] && [ -n "$BASE_SHA" ] && git -C "$WORKSPACE" cat-file -e "$BASE_SHA^{commit}" 2>/dev/null && git -C "$WORKSPACE" merge-base "$BASE_SHA" HEAD >/dev/null 2>&1; then
  echo "  PASS: Git 工作树包含 base/head 与 merge-base 历史"
  PASS=$((PASS+1))
else
  echo "  FAIL: Git 工作树历史不可用: workspace=$WORKSPACE base=$BASE_SHA"
  git -C "$WORKSPACE" rev-parse HEAD || true
  git -C "$WORKSPACE" merge-base "$BASE_SHA" HEAD || true
  FAIL=$((FAIL+1))
fi

cmd "kimiinput /help"
cmd "kimienter"
sleep 2
assert "真实 CLI 可接收斜杠命令且保持运行" "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_closed')==False and d.get('kimi_terminal_error')==''"

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

OTHER_REPO="${PD_REPO2:-octocat/Hello-World}"
OTHER_NUMBER="${PD_NUMBER2:-2}"
python - "$CONTEXT" "$OTHER_REPO" "$OTHER_NUMBER" <<'PY'
import hashlib, json, pathlib, shutil, sqlite3, sys
source = pathlib.Path(sys.argv[1])
repo, number = sys.argv[2], int(sys.argv[3])
pr = json.loads((source / "pr.json").read_text())
base, head = pr["base_sha"], pr["head_sha"]
identity = f"{repo}#{number}@{base}..{head}"
digest = hashlib.sha256(identity.encode()).hexdigest()[:16]
destination = source.parent / f"pr-desk-acp-context-v2-{digest}"
if not destination.exists():
    shutil.copytree(source, destination)
for name in ("pr.json", "context.json"):
    path = destination / name
    data = json.loads(path.read_text())
    data["repo"] = repo
    data["number"] = number
    path.write_text(json.dumps(data, indent=2))
db_path = source.parent.parent / "pr-desk.db"
with sqlite3.connect(db_path) as db:
    db.execute("DELETE FROM deleted_prs WHERE repo=? AND number=?", (repo, number))
    db.execute(
        "INSERT INTO prs(repo,number,title,author,url,head_sha,status,tags,role,source) VALUES(?,?,?,?,?,?,?,?,?,?) "
        "ON CONFLICT(repo,number) DO UPDATE SET title=excluded.title,head_sha=excluded.head_sha,status='pending'",
        (repo, number, f"Offline fixture #{number}", "octocat", f"https://github.com/{repo}/pull/{number}", head, "pending", "[]", "reviewer", "inbox"),
    )
PY
cmd "openpr $OTHER_REPO $OTHER_NUMBER"
wait_state "d['panel_open']==True and d.get('panel_number')==$OTHER_NUMBER" || true
assert "切到另一个 PR 后原 CLI 仍在后台" "d.get('kimi_cached_live')>=1"
cmd "tab 4"
wait_state "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False" || true
OTHER_REQUEST_ID=$(state | python -c "import json,sys; print(json.load(sys.stdin).get('kimi_request_id') or 0)")
assert "第二个 PR 使用独立 CLI" "$OTHER_REQUEST_ID > 0 and $OTHER_REQUEST_ID != $REQUEST_ID and d.get('kimi_terminal_active')==True and d.get('kimi_terminal_error')==''"

cmd "openpr $REPO $NUMBER"
wait_state "d['panel_open']==True and d.get('panel_number')==$NUMBER" || true
assert "返回时另一个 CLI 保留在后台" "d.get('kimi_cached_live')>=1"
cmd "tab 4"
wait_state "d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False" || true
assert "切回立即恢复原 CLI 进程" "d.get('kimi_request_id')==$REQUEST_ID and d.get('kimi_terminal_active')==True and d.get('kimi_terminal_preparing')==False and d.get('kimi_terminal_error')==''"
assert "两个 PR 的 CLI 可同时存活" "d.get('kimi_cached_live')==1 and d.get('kimi_terminal_closed')==False"

cmd "review"
sleep 1
assert "普通预评审入口仍然可用" "d['panel_open']==True and (d.get('panel_review_status') in ('running','done','failed') or 'LLM' in d.get('status_msg','') or '未配置' in d.get('status_msg','') or '拉取' in d.get('status_msg','') or 'Fetching' in d.get('status_msg',''))"

cmd "agentreview"
sleep 2
assert "后台 Kimi 深审进入持久队列" "d.get('agent_review_status') in ('queued','preparing','running','validating','waiting_decision','completed','failed','superseded')"
cmd "needsyou"
sleep 1
assert "Needs-you 视图已接入" "d['view_mode']==2 and d.get('needs_you_count') is not None"

echo "=== 结果: $PASS pass / $FAIL fail ==="
[ $FAIL -eq 0 ]
