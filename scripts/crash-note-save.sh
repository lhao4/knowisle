#!/usr/bin/env bash
#
# 崩溃验收，两个互补的问题：
#
#   1. **原子性**：写入途中被硬杀，磁盘上只允许出现完整的旧内容或完整的新内容。
#   2. **丢失量**：一直在打字时被硬杀，最多丢多久的输入。
#
# 第一个问题回答"会不会写坏"，第二个回答"用户断电丢多少"。
# 一个文件可以每字节都完整，却仍然落后于用户手指 10 分钟——所以两个都要验。
#
# 用法：
#
#   ./scripts/crash-note-save.sh              # 各 20 轮
#   KI_CRASH_ROUNDS=200 ./scripts/crash-note-save.sh
#
# # 为什么是脚本而不是一段 shell 里的 kill -9 循环
#
# 硬杀发生在**子进程内部**（`Child::kill()`：Unix 上是 SIGKILL，也就是 kill -9；
# Windows 上是 TerminateProcess）。放在 shell 里做 kill -9 需要再实现一套
# "写到哪一代了"的协议，等于把同一个不变量验两遍、还只验对一遍。
# 这里只做一层包装：保证用正确的参数跑那两条用例。

set -euo pipefail

cd "$(dirname "$0")/.."

ROUNDS="${KI_CRASH_ROUNDS:-20}"

# 硬杀机制随平台不同（Unix 是 SIGKILL，Windows 是 TerminateProcess），
# 输出里写清楚，免得看到一堆 gen= 却不知道验的是哪条路径。
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) PLATFORM="Windows（TerminateProcess）" ;;
  Darwin)                   PLATFORM="macOS（SIGKILL）" ;;
  *)                        PLATFORM="Linux（SIGKILL）" ;;
esac

echo "════ 1/2 原子性：${ROUNDS} 轮，每轮写 1MB 后在随机时刻硬杀"
echo "平台：${PLATFORM}"
echo

# --ignored：这两条用例默认被跳过，它们要起外部进程且依赖时序
# --nocapture：否则看不到每轮的结果行
KI_CRASH_ROUNDS="$ROUNDS" \
  cargo test -p ki-vault --test crash -- --ignored --nocapture

echo
echo "════ 2/2 丢失量：${ROUNDS} 轮，持续打字时硬杀，量「丢了多少毫秒」"
echo

KI_TYPING_ROUNDS="$ROUNDS" \
  cargo test -p ki-ipc --test crash_autosave -- --ignored --nocapture
