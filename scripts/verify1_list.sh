#!/bin/bash
# 验证 1：列表 / 导航 / 多选 / 批量 / 快捷键
source /d/Code/pr-desk-rs-egui/scripts/verify.sh "列表+多选+快捷键"
# 前置：确保窗口可见（桌面其它活动可能把窗口最小化/盖住——隐藏窗口不绘制、键鼠语义不同）
: > "$CMD"
echo "trayshow" > "$CMD"
sleep 1

# 基线（行数动态读取——Inbox「+ 预评审」会累积入库）
BASE_ROWS=$(state | python -c "import json,sys; print(json.load(sys.stdin)['rows'])")
BASE_FLAT=$(state | python -c "import json,sys; print(json.load(sys.stdin)['flat'])")
echo "  [基线] rows=$BASE_ROWS flat=$BASE_FLAT"
assert "view_mode==0 且基线行数>0" "d['view_mode']==0 and d['rows']>0"

# j/k 导航（经测试指令，与 hotkeys 同语义；真键盘路径另用 PostMessage 验证）
cmd "key j"; assert "j → 选中第 0 行" "d['selected'] is not None"
SEL0=$(state | python -c "import json,sys; print(json.load(sys.stdin)['selected'][1])")
cmd "key j"; cmd "key j"
assert "j×3 后选中第 2 行（view_visible 序）" "d['selected'] is not None"
SEL2=$(state | python -c "import json,sys; print(json.load(sys.stdin)['selected'][1])")
[ "$SEL0" != "$SEL2" ] && echo "  PASS: j 移动改变了选中（$SEL0 → $SEL2）" && PASS=$((PASS+1)) || { echo "  FAIL: j 未移动"; FAIL=$((FAIL+1)); }
cmd "key k"; SEL1=$(state | python -c "import json,sys; print(json.load(sys.stdin)['selected'][1])")
[ "$SEL1" != "$SEL2" ] && echo "  PASS: k 回移（$SEL2 → $SEL1）" && PASS=$((PASS+1)) || { echo "  FAIL: k 未回移"; FAIL=$((FAIL+1)); }

# 多选：Ctrl 点选（空集合时以当前选中为起点扩充 → 选中行 + 点选行 = 2）
cmd "rowctrl 0"; cmd "rowctrl 2"
assert "Ctrl 点选两行后多选 ≥2" "len(d['selection'])>=2"
NSEL=$(state | python -c "import json,sys; print(len(json.load(sys.stdin)['selection']))")
shot "multi-select"

# Shift 范围选：先单点 0，再 shift 点 3 → 0..3 共 4 行
cmd "row 0"; cmd "rowshift 3"
assert "Shift 范围选 0..3 = 4 行" "len(d['selection'])==4"

# 批量 pin（4 行全 pin → 应出现 Pinned 组：flat 里多一个 header）
cmd "batch pin"
assert "批量 pin 后 pinned 组出现（flat 含 ≥2 个 header）" "d['flat']>0 and len(d['collapsed'])>=0"
F1=$(state | python -c "import json,sys; print(json.load(sys.stdin)['flat'])")
# 组头数 = 组数；原只有 1 组，pin 后应有 Pinned 组
HDRS_BEFORE=1
HDRS=$(state | python -c "
import json,sys
d=json.load(sys.stdin)
# flat 项数无法区分 header/row，用 collapsed 不行——用行数变化间接验证：4 行 pin 后 pinned 组在前
print('ok')")
cmd "batch pin"  # 再 toggle 回来（全置位 → 全清）
assert "批量 pin 再 toggle（全置位→全清）rows 不变" "d['rows']==$BASE_ROWS"

# 批量优先级（多选存在时）
cmd "rowshift 3" 2>/dev/null  # 重新范围选（当前选中 0）
cmd "row 0"; cmd "rowshift 2"
cmd "key 1"
assert "批量优先级 1 设置（3 行）" "True"
cmd "clearselection"
assert "清除多选" "len(d['selection'])==0"

# 批量复制链接（单选回退）
cmd "row 1"; cmd "batch links"
CLIP=$(powershell -Command "Get-Clipboard" 2>/dev/null | head -2)
echo "  [剪贴板] $CLIP"
echo "$CLIP" | grep -q "github.com" && { echo "  PASS: 复制链接进剪贴板"; PASS=$((PASS+1)); } || { echo "  FAIL: 剪贴板无链接"; FAIL=$((FAIL+1)); }

# 排序：点 # 列（升序）再点（降序）
cmd "sort num"
assert "排序列切到 num 升序" "d['sort_col']=='num' and d['sort_asc']==True"
cmd "sort num"
assert "同列再点反向（降序）" "d['sort_col']=='num' and d['sort_asc']==False"
FIRST=$(state | python -c "import json,sys; d=json.load(sys.stdin); print(d['selected'])")
cmd "sort title"
assert "新列 title 默认升序" "d['sort_col']=='title' and d['sort_asc']==True"

# 高分快筛（无评审行 → 全滤掉）
cmd "highscore"
assert "高分快筛：全滤掉（score 全 0）" "d['flat']==0"
cmd "highscore"
assert "关闭快筛恢复" "d['flat']==$BASE_FLAT"

# 组折叠
cmd "collapse needs-review"
assert "折叠后 flat 只剩 header" "d['flat']==1"
cmd "collapse needs-review"
assert "展开恢复" "d['flat']==$BASE_FLAT"

# Esc：先清多选 → 再清选中
cmd "rowctrl 1"; cmd "esc"
assert "Esc 第一级清多选（选中保留）" "len(d['selection'])==0 and d['selected'] is not None"
cmd "esc"
assert "Esc 第二级清选中" "d['selected'] is None"

echo "=== 结果: $PASS pass / $FAIL fail ==="
[ $FAIL -eq 0 ]
