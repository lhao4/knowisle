#!/usr/bin/env bash
#
# 崩溃一致性验收：写入途中被硬杀，磁盘上只允许出现完整的旧内容或完整的新内容。
#
# M1 唯一不可妥协的验收项就是这一条，不是"能打字"。用法：
#
#   ./scripts/crash-note-save.sh              # 默认 20 轮
#   KI_CRASH_ROUNDS=200 ./scripts/crash-note-save.sh
#
# # 为什么是脚本而不是一段 shell 里的 kill -9 循环
#
# 硬杀发生在**子进程内部**（`Child::kill()`：Unix 上是 SIGKILL，也就是 kill -9；
# Windows 上是 TerminateProcess）。放在 shell 里做 kill -9 需要再实现一套
# "写到哪一代了"的协议，等于把同一个不变量验两遍、还只验对一遍。
# 这里只做一层包装：保证用正确的参数跑那条用例。
#
# 输出里每一轮的 `gen` 是崩溃后磁盘上活下来的代次，`leftovers` 是孤儿 tmp 数量。
# 判据只有一条：**任何一轮都不允许出现"长度不对"或"内容对不上"**。

set -euo pipefail

cd "$(dirname "$0")/.."

ROUNDS="${KI_CRASH_ROUNDS:-20}"

echo "崩溃一致性测试：${ROUNDS} 轮，每轮写 1MB 后在随机时刻硬杀"
echo "平台：$(uname -s)   （Unix 用 SIGKILL，Windows 用 TerminateProcess）"
echo

# --ignored：这条用例默认被跳过，因为它要起外部进程且依赖时序
# --nocapture：否则看不到每轮的结果行
KI_CRASH_ROUNDS="$ROUNDS" \
  cargo test -p ki-vault --test crash -- --ignored --nocapture
