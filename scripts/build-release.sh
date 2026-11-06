#!/usr/bin/env bash
# 低并发 release 构建：Slint 全量编译内存峰值高，Windows 页面文件不足时
# rustc/ld 会崩（STATUS_STACK_BUFFER_OVERRUN / os error 1455）。
# -j2 + 关增量是内存与速度的折中；还爆就 -j1。
set -euo pipefail
cd "$(dirname "$0")/.."
CARGO_INCREMENTAL=0 cargo build --release -j2 "$@"
