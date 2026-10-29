/**
 * 批量转存的**纯逻辑**：把结果折算成一句话，把进度折算成一个百分比。
 *
 * 放在 `lib/` 而不是组件里，是因为这两件事都有边界值需要被断言
 * （0 篇 / 全失败 / 取消在中途），而组件测试在本项目里不做（ARCHITECTURE 1.7）。
 */

/** 一次批量转存的规模。与 `TranscodeBatch` 的前三个字段同构。 */
export interface BatchTally {
  /** 真的被改写成 UTF-8 的篇数。 */
  ok: number
  /** 失败的篇数。 */
  failed: number
  /** 用户中途点了取消。 */
  cancelled: boolean
}

/**
 * 结果那一行字。
 *
 * # 取消**不是**失败
 *
 * 取消之后仍然可能已经转好了 300 篇——那是实打实的成果，必须说出去。
 * 把它写成「已取消」三个字，用户会以为一篇都没转。
 *
 * # 全失败与部分失败分开说
 *
 * 「0 篇成功 / 20 篇失败」和「17 篇成功 / 3 篇失败」是两种处境：前者该去查
 * 权限，后者该去看那三条。用同一句话概括会让第一种看起来像「大部分成功了」。
 */
export function summarizeBatch(tally: BatchTally): string {
  const { ok, failed, cancelled } = tally

  if (cancelled) {
    if (ok === 0 && failed === 0) return '已取消，一篇都没有转存'
    if (failed === 0) return `已取消：${ok} 篇已经转存为 UTF-8`
    return `已取消：${ok} 篇已转存，${failed} 篇失败`
  }

  if (failed === 0) return `${ok} 篇全部转存为 UTF-8`
  if (ok === 0) return `${failed} 篇全部失败`
  return `${ok} 篇成功 / ${failed} 篇失败`
}

/**
 * 进度百分比，钳在 0–100。
 *
 * `total` 为 0 时返回 0 而不是 `NaN`：`0/0` 会算出 `NaN`，而 `NaN%` 会让
 * 进度条的宽度变成一个非法值（`width: NaN%` 被浏览器丢掉，看起来像
 * 「进度条消失了」）。
 */
export function progressPercent(done: number, total: number): number {
  if (total <= 0) return 0
  const ratio = done / total
  if (!Number.isFinite(ratio)) return 0
  return Math.min(100, Math.max(0, Math.round(ratio * 100)))
}

/**
 * 「索引还在建」时的说明。
 *
 * 这个数会随着索引建完而变大。不解释的话，用户看到 0 会以为库里没有
 * 非 UTF-8——而那正是这一刀要消灭的「不知道」。
 */
export function buildingHint(done: number, total: number): string {
  if (total <= 0) return '索引正在建立，这个数还会变。'
  return `索引正在建立（${done}/${total}），这个数还会变。`
}
