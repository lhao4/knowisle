import { useCallback, useEffect, useRef, useState } from 'react'

import { describeError, vaultSkippedNotes, type IndexSkipStats, type ScanResult } from '../api'

/**
 * 索引还在建时多久问一次。
 *
 * 与 `useEncoding` 同一个量级（1.5 秒）：建库是 10–108 秒的事，1.5 秒足够让
 * 「数会长」这个事实可见，又不至于变成轮询风暴（一次查询是 `count(*)` 加最多
 * 50 行）。
 */
const BUILDING_POLL_MS = 1500

interface UseSkipListOptions {
  /** 当前库。`null` 时所有动作都是空操作。 */
  vault: ScanResult | null
}

/**
 * 跳过清单的**索引侧**那一半：哪几篇没进索引、为什么。
 *
 * # 它只管一半
 *
 * 扫描侧的清单**不在这里**——它跟着 `vault`（`ScanResult.skippedDetail`）
 * 一起来，`vault_scan` 的返回值里就有。再发一次 IPC 去问同一件事，等于让
 * 两份数据有机会在两次调用之间漂移。
 *
 * 所以这个 hook 只负责索引侧：那条数据只能查索引库（`vault_skipped_notes`），
 * 而索引是后台建的，得轮询。
 *
 * # 为什么它不是第四个「域」
 *
 * 判据是「读写都在一个域 → 进 hook」。这里读的是索引库、写的是面板开合，
 * 两件事都只属于它自己；跨域的那一步（面板里要显示扫描侧的数）通过参数拿
 * （`vault`），不塞进 hook。
 */
export function useSkipList({ vault }: UseSkipListOptions) {
  /**
   * 最近一次索引侧统计。`null` = 还没查过（与「查过、库里没有」不同）。
   *
   * 入口是否出现**不只看它**：扫描侧有跳过时入口也要出现，而那个数在
   * `vault` 里。判据写在 `App.tsx`，因为只有那里同时看得到两边。
   */
  const [stats, setStats] = useState<IndexSkipStats | null>(null)
  const [open, setOpen] = useState(false)
  const [error, setError] = useState<string | null>(null)
  /** 面板容器。关闭后把焦点还给编辑器。 */
  const panelRef = useRef<HTMLDivElement>(null)

  /**
   * 重读索引侧统计。
   *
   * `root` 走参数而不是闭包里的 `vault`：这样这个函数是稳定的，
   * `applyVault` 的依赖数组里不会因为它每次重建（`useEncoding.refresh` 同款）。
   */
  const refresh = useCallback(async (root: string) => {
    try {
      setStats(await vaultSkippedNotes(root))
      setError(null)
    } catch (err) {
      // 读不出来不该打断编辑：它是旁路信息。面板打开时用户一定会看见。
      setError(describeError(err))
    }
  }, [])

  /** 换到「没有库」：入口消失。 */
  const reset = useCallback(() => {
    setStats(null)
    setOpen(false)
    setError(null)
  }, [])

  /**
   * 索引还在建时继续问，直到它建完。
   *
   * 没有这一段，`stats` 就只在「扫描刚结束」那一刻取一次——而那时后台索引
   * 通常还没建完，`total` 是 0，**入口永远不出现**。schema 升级之后每个老用户
   * 都会撞上这条路（整库重建）。第十一刀（ADR-012）已经踩过这个坑。
   */
  useEffect(() => {
    if (vault === null) return
    if (stats === null) return
    if (stats.indexState.state !== 'building') return

    const timer = setTimeout(() => void refresh(vault.root), BUILDING_POLL_MS)
    return () => clearTimeout(timer)
  }, [vault, stats, refresh])

  /**
   * 打开面板：先开，再重读。
   *
   * 顺序与回收站 / 待转存一致（点了就有反应），重读是为了「上一次关掉之后
   * 索引又变过」这种情况不显示旧清单。
   */
  const openPanel = useCallback(() => {
    if (!vault) return
    setError(null)
    setOpen(true)
    void refresh(vault.root)
  }, [vault, refresh])

  const closePanel = useCallback(() => setOpen(false), [])

  return {
    stats,
    open,
    error,
    panelRef,
    refresh,
    reset,
    openPanel,
    closePanel,
  }
}
