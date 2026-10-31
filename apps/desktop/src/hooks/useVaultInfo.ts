import { useCallback, useEffect, useRef, useState } from 'react'

import { describeError, vaultInfo, type ScanResult, type VaultInfo } from '../api'

/**
 * 索引还在建时多久问一次。
 *
 * 与 `useSkipList` / `useEncoding` 同一个量级（1.5 秒）：建库是 10–108 秒的事，
 * 1.5 秒足够让「还在建」这个事实可见，又不至于变成轮询风暴——这里每次只是一次
 * `read_dir` + 几次 `metadata`，不开库、不读正文。
 */
const BUILDING_POLL_MS = 1500

interface UseVaultInfoOptions {
  /** 当前库。`null` 时所有动作都是空操作。 */
  vault: ScanResult | null
}

/**
 * 库信息面板的那一半：**只读快照**，打开时取一次。
 *
 * # 与 `useSkipList` 的三处不同（都是刻意的）
 *
 * 1. **入口常驻，不看数据**。「跳过 N」只在非零时出现，因为 0 条等于没有话要说；
 *    而「库信息」任何时候都有话要说——它就是「这个库现在是什么状态」。
 * 2. **只在面板打开时续问**。跳过清单要为了「入口该不该出现」而在后台一直问；
 *    这里没有那个需求，面板关着就不该有 IPC。
 * 3. **失败不静默**。库根读不出来时把 `describeError` 的文案放进面板，而不是
 *    像旁路信息那样吞掉——用户是主动点开这个面板的，沉默等于让他以为库是空的。
 *
 * # 它为什么是一个域
 *
 * 读的是 `vault_info`、写的是面板开合与那份快照，两件事都只属于它自己。
 * 跨域的那一步（面板里要显示扫描结果里的笔记数、目录数）通过参数拿（`vault`），
 * 不塞进 hook——与 `useSkipList` 同一条判据。
 */
export function useVaultInfo({ vault }: UseVaultInfoOptions) {
  /** 最近一次快照。`null` = 还没取过（与「取过、都是 0」不同）。 */
  const [info, setInfo] = useState<VaultInfo | null>(null)
  const [open, setOpen] = useState(false)
  const [error, setError] = useState<string | null>(null)
  /** 正在取。只用于把「刷新」按钮置灰，不参与布局。 */
  const [busy, setBusy] = useState(false)
  /** 面板容器。关闭后把焦点还给编辑器。 */
  const panelRef = useRef<HTMLDivElement>(null)

  /**
   * 取一次快照。
   *
   * `root` 走参数而不是闭包里的 `vault`：这样这个函数是稳定的，
   * 依赖数组里不会因为它每次重建（`useSkipList.refresh` 同款）。
   *
   * 失败时**保留上一份** `info`：面板上「上次看到的 1.2 MB」比「什么都没有」
   * 更有用，而错误行会明说这一次没读成。
   */
  const refresh = useCallback(async (root: string) => {
    setBusy(true)
    try {
      setInfo(await vaultInfo(root))
      setError(null)
    } catch (err) {
      setError(describeError(err))
    } finally {
      setBusy(false)
    }
  }, [])

  /** 换到「没有库」：入口消失、快照作废。 */
  const reset = useCallback(() => {
    setInfo(null)
    setOpen(false)
    setError(null)
    setBusy(false)
  }, [])

  /**
   * 索引还在建时继续问，直到它建完。
   *
   * **只在面板打开时问**：关着的时候用户看不见这个数，问了也没有人受益，
   * 而一次 IPC 就是一次磁盘 stat。面板重新打开时会先取一次，不会显示旧值。
   */
  useEffect(() => {
    if (!open) return
    if (vault === null) return
    if (info === null) return
    if (info.indexState.state !== 'building') return

    const timer = setTimeout(() => void refresh(vault.root), BUILDING_POLL_MS)
    return () => clearTimeout(timer)
  }, [open, vault, info, refresh])

  /**
   * 打开面板：先开，再取。
   *
   * 顺序与回收站 / 跳过 / 待转存一致（点了就有反应），取是为了「上一次关掉
   * 之后磁盘又变过」这种情况不显示旧数字。
   */
  const openPanel = useCallback(() => {
    if (!vault) return
    setError(null)
    setOpen(true)
    void refresh(vault.root)
  }, [vault, refresh])

  const closePanel = useCallback(() => setOpen(false), [])

  return {
    info,
    open,
    error,
    busy,
    panelRef,
    refresh,
    reset,
    openPanel,
    closePanel,
  }
}
