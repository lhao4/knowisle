import type { IgnoreConfigError, IgnoreRules } from '../api'

/**
 * 内置名单里的一条，以及它是否被用户停用。
 *
 * # 为什么要在前端算「停用」
 *
 * `IgnoreRules.builtin` 是**原文**（15 条，`negate` 不从里面删）。这是刻意的：
 * 删掉的话面板只能显示「剩下 14 条」，而用户改完配置最想确认的恰恰是
 * 「我 negate 的那一条真的不生效了吗」。所以停用态必须单独标出来。
 */
export interface BuiltinRule {
  name: string
  disabled: boolean
}

/**
 * 把内置名单与 `negate` 合起来，得到「每条规则是否生效」。
 *
 * 比较走 `toLowerCase()`：Rust 侧是 `eq_ignore_ascii_case`，两边必须同一条
 * 判定。只折 ASCII 就够——规则名是目录名，而 `negate` 能停用的只有内置名单
 * 里那 15 个 ASCII 名字。
 */
export function splitBuiltin(rules: IgnoreRules): BuiltinRule[] {
  const off = new Set(rules.negate.map((name) => name.toLowerCase()))
  return rules.builtin.map((name) => ({
    name,
    disabled: off.has(name.toLowerCase()),
  }))
}

/**
 * 配置错误压成一行。
 *
 * 行号能拿到就给（手写的文件没有行号就只能通读一遍）；拿不到时（整个文件
 * 读不出来）只说人话，不补一个假的「第 0 行」。
 */
export function configErrorLine(error: IgnoreConfigError): string {
  return error.line === null ? error.message : `第 ${error.line} 行：${error.message}`
}

/**
 * 有没有用户自己写的规则（不含错误）。
 *
 * 面板用它决定「用户规则」那一栏是说「没有」还是列出来——空列表配一句
 * 「没有」比什么都不显示更清楚：用户会以为渲染坏了。
 */
export function hasUserRules(rules: IgnoreRules): boolean {
  return rules.extra.length > 0 || rules.negate.length > 0
}
