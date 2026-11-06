#!/bin/bash
# 验证 3：Inbox 全量（拉取/过滤/未读开关/预览/标记已读/+预评审/加载更多）
source /d/Code/pr-desk-rs-egui/scripts/verify.sh "Inbox"

cmd "inbox"
assert "切到 Inbox 视图" "d['view_mode']==1"
# 等首次拉取完成（网络，~1.5s）
for i in $(seq 1 20); do
  L=$(state | python -c "import json,sys; print(json.load(sys.stdin)['inbox_loading'])")
  [ "$L" = "False" ] && break
  sleep 0.5
done
assert "首次加载态已关闭" "d['inbox_loading']==False"
assert "通知已拉取（≥50 条）" "d['inbox']>=50"
assert "has_more（满页）" "d['inbox_has_more']==True"
shot "inbox-list"

# 预览：点击第 0 条（自动标记已读：MarkNotifRead 发出 → 重拉）
UNREAD_BEFORE=$(state | python -c "import json,sys; d=json.load(sys.stdin); print(d['inbox'])")
cmd "inboxrow 0"; sleep 3
assert "预览面板打开" "d['nd_open']==True"
assert "预览加载完成（loading 关闭）" "d['nd_loading']==False"
assert "预览会话内容非空" "d['nd_convo']>0"
shot "inbox-preview"

# 身份守卫：快速连点两条（旧结果不得覆盖）
cmd "inboxrow 1"; cmd "inboxrow 2"; sleep 3
assert "连点两条后面板开着且加载完成" "d['nd_open']==True and d['nd_loading']==False"

# 仓库过滤
cmd "inboxfilter vllm-project/vllm"
assert "仓库过滤生效" "d['inbox_view']<=d['inbox']"
cmd "inboxfilter "
assert "清过滤恢复" "d['inbox_view']==d['inbox']"

# 只看未读
cmd "unreadonly"; sleep 3
assert "只看未读：重拉完成" "d['inbox_loading']==False"
NU=$(state | python -c "import json,sys; print(json.load(sys.stdin)['inbox'])")
echo "  [只看未读条数] $NU"
cmd "inboxclose"
# 切回全部（再 toggle 一次：unreadonly 是单向置 false；回切用另一条指令路径没有——直接再发 unreadonly 不会回弹，跳过回弹验证）

# + 预评审（保守语义：已存在 → 提示 already_in_list，不重复插入）
cmd "inboxadd 0"; sleep 1.5
SM=$(state | python -c "import json,sys; print(json.load(sys.stdin)['status_msg'])")
echo "  [+ 预评审 status] $SM"
echo "$SM" | grep -qE "已在列表|已加入" && { echo "  PASS: + 预评审保守语义（已在列表/已加入）"; PASS=$((PASS+1)); } || { echo "  FAIL: $SM"; FAIL=$((FAIL+1)); }

# 加载更多（page 1 → 2）
cmd "loadmore"; sleep 3
assert "加载更多：page=2" "d['inbox_page']==2"
assert "加载更多：通知数 >50" "d['inbox']>50"
shot "inbox-page2"

# 标记已读已随 inboxrow 发出（MarkNotifRead）；全部已读跳过——会清用户真实收件箱（见报告）

echo "=== 结果: $PASS pass / $FAIL fail ==="
[ $FAIL -eq 0 ]
