#!/bin/bash
# PR Desk egui 全量验证驱动（PR_DESK_TEST=1 实例 + 指令文件 + 状态快照 + F12 截图）
T="${PR_DESK_TEST_DIR:-/c/Users/20370/AppData/Local/Temp}"
CMD="$T/prdesk-cmd.txt"
STATE="$T/prdesk-state.json"
SHOT="$T/prdesk-shot.png"
LOG="${PD_LOG:-/tmp/pd.log}"
SHOT_DIR="${PD_SHOT_DIR:-/tmp/pd-shots}"
mkdir -p "$SHOT_DIR"

cmd() {
  local before
  before=$(grep -c "test-cmd 已处理" "$LOG" 2>/dev/null || true)
  before=${before:-0}
  : > "$CMD"
  echo "$1" > "$CMD"
  # 等本条指令被处理（回执计数增加，防匹配到历史行）
  for i in $(seq 1 50); do
    local now
    now=$(grep -c "test-cmd 已处理" "$LOG" 2>/dev/null || echo 0)
    [ "$now" -gt "$before" ] && break
    sleep 0.2
  done
  sleep 0.3
}

shot() {
  local before
  before=$(grep -c "截图已存" "$LOG" 2>/dev/null || true)
  before=${before:-0}
  : > "$CMD"
  echo "shot" > "$CMD"
  for i in $(seq 1 30); do
    local now
    now=$(grep -c "截图已存" "$LOG" 2>/dev/null)
    now=${now:-0}
    [ "$now" -gt "$before" ] && break
    sleep 0.3
  done
  if [ -f "$SHOT" ]; then
    cp "$SHOT" "$SHOT_DIR/$1.png" && echo "  [shot] $1 ($(stat -c%s "$SHOT_DIR/$1.png") bytes)"
  else
    echo "  [shot] $1 skipped (no drawable surface)"
  fi
}

state() {
  : > "$CMD"
  echo "dumpstate" > "$CMD"
  sleep 0.8
  cat "$STATE"
}

# assert <jq-expr-true> <label>
PASS=0; FAIL=0
assert() {
  local label="$1"; shift
  local expr="$1"
  local v
  v=$(state | python -c "import json,sys; d=json.load(sys.stdin); print(1 if ($expr) else 0)" 2>/dev/null)
  if [ "$v" = "1" ]; then PASS=$((PASS+1)); echo "  PASS: $label";
  else FAIL=$((FAIL+1)); echo "  FAIL: $label"; state | head -50; fi
}

echo "=== $1 ==="
