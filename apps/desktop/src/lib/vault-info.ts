/**
 * 库信息面板的**纯逻辑**：把索引状态翻成一句人话。
 *
 * # 为什么单独抽出来
 *
 * 四种状态的文案是这个面板最容易写错、也最容易被改坏的地方：`building` 时
 * 少说一句「可能不全」，用户就会把搜索结果当成全部；`failed` 时把 `message`
 * 吞掉，用户就只剩一个「失败」可看。这两种错都不会报错，只会让人误解。
 *
 * 组件测试在本项目里不做（ARCHITECTURE 1.7），所以约束落在这里。
 */

import type { IndexState } from '../api'

/** 一个状态的两种说法：一行标签 + 一句解释。 */
export interface IndexStateCopy {
  /** 短标签，直接放在「索引」那一行的右边。 */
  label: string
  /** 一句解释。空字符串表示不需要解释。 */
  detail: string
}

/**
 * 索引状态 → 文案。
 *
 * 四种状态各有各的**下一步动作**，所以文案不能只换形容词：
 *
 * - `idle`：本进程还没碰过索引。**不代表索引是空的**——上一个进程可能已经建好了。
 * - `building`：正在建，搜索结果可能不全。
 * - `ready`：对齐了，结果完整。
 * - `failed`：建失败了，但**已建好的那部分仍然是对的**（不该因此清空结果）。
 */
export function describeIndexState(state: IndexState): IndexStateCopy {
  switch (state.state) {
    case 'idle':
      return {
        label: '还没开始',
        detail: '本进程还没为这个库建过索引。这不代表索引是空的——上一个进程可能已经建好了，第一次搜索时会对齐。',
      }
    case 'building':
      return {
        label: `正在建立 ${state.done} / ${state.total} 篇`,
        detail: '建完之前搜索结果可能不全。这个数会自己往前走，不用管它。',
      }
    case 'ready':
      return {
        label: '已就绪',
        detail: '索引已对齐到最近一次扫描，搜索结果完整。',
      }
    case 'failed':
      return {
        label: '建立失败',
        detail: `已经建好的那部分仍然是对的，能搜到的结果不受影响。原因：${state.message}`,
      }
  }
  // 与 `describeError` 同一条约束：**没有 `default`**，新变体加进来时这行会
  // 因 `state` 不再是 `never` 而编译失败，而不是静默落到某个兜底文案上。
  const _exhaustive: never = state
  void _exhaustive
  return { label: '未知', detail: '' }
}

/**
 * 扫描耗时的说法。
 *
 * 只做一件事：把 `0` 与「很快」区分开。`elapsedMs === 0` 在真实扫描里不会
 * 出现（哪怕空库也要几毫秒），出现就说明这个数不是量出来的——说「不到 1 ms」
 * 比说「0 ms」诚实。
 */
export function formatScanMs(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '未知'
  if (ms < 1) return '不到 1 ms'
  return `${Math.round(ms)} ms`
}
