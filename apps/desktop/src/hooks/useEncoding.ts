import { useCallback, useEffect, useRef, useState } from 'react'

import {
  describeError,
  noteTranscodeBatch,
  noteTranscodeCancel,
  noteTranscodeProgress,
  vaultEncodingStats,
  type EncodingStats,
  type ScanResult,
  type TranscodeBatch,
  type TranscodeProgress,
} from '../api'

/**
 * 索引还在建时多久问一次。
 *
 * 建库是 10–108 秒的量级，1.5 秒的粒度足够让「数会长」这个事实可见，又不至于
 * 变成轮询风暴（一次查询是索引上的 `count(*)` + 最多 1000 行）。
 */
const BUILDING_POLL_MS = 1500

/**
 * 批量转存期间多久问一次进度。
 *
 * 一篇转存 15–36ms（release 实测），所以 120ms 一次意味着进度条每跳一格
 * 背后是 3–8 篇——够平滑，也不至于把 IPC 变成瓶颈。
 */
const PROGRESS_POLL_MS = 120

interface UseEncodingOptions {
  /** 当前库。`null` 时所有动作都是空操作。 */
  vault: ScanResult | null
  /**
   * 开始批量**之前**要做的事（把编辑器缓冲落盘）。
   *
   * 批量的 `expected_mtime` 是 `None`（按磁盘现状转），所以「编辑器里压着的
   * 那几个字」必须先进磁盘，否则转存读到的还是旧内容——转完再保存会是一次
   * 覆盖，用户会以为转存丢字。
   */
  beforeStart: () => Promise<void>
  /**
   * 转存结束之后，若当前打开的那篇在其中，刷新编辑器。
   *
   * 跨域（要动 `activeNote` / `docId` / 自动保存槽位），所以由 `App.tsx`
   * 提供——判据与 `useTrash.restore(entry, after)` 完全一样：hook 只管
   * 自己那一域的 IPC 与状态位。
   */
  onTranscoded: (paths: string[]) => Promise<void>
}

/**
 * 编码域：库级「N 篇不是 UTF-8」、待转存清单、批量转存。
 *
 * # 为什么它必须是独立的一域
 *
 * 读的是索引库（`vault_encoding_stats`），写的是磁盘编码（`note_transcode_batch`），
 * 两件事都不属于侧栏、检索或回收站。放进任一域都要给另外两个域开新口子。
 *
 * # 面板关掉不等于停下
 *
 * 1000 篇要跑三十几秒。把它做成「关不掉的模态」等于让用户在转存期间不能
 * 编辑——而这一刀的全部意义是「一次处理完」，不是「盯着它转」。
 * 所以 `panelOpen` 与 `running` 是**两个独立的状态**：面板可以关，批次继续跑，
 * 侧栏入口变成「转存中 320/1000」，再打开还能看到同一条进度、同一个取消按钮。
 *
 * # 进度从哪来
 *
 * 每 120ms 问一次 Rust（`note_transcode_progress`）。**不许**按「发了几个
 * 请求」猜：一次 IPC 调用跑完 1000 篇，中间没有任何可观测的点。
 */
export function useEncoding({ vault, beforeStart, onTranscoded }: UseEncodingOptions) {
  /**
   * 最近一次统计。`null` = 还没查过（与「查过、库里没有」不同）。
   *
   * 入口是否出现只认 `stats.total > 0`，不认「有没有打开过库」之类的近似。
   */
  const [stats, setStats] = useState<EncodingStats | null>(null)
  const [open, setOpen] = useState(false)
  /** 二次确认是否打开（复用 `ConfirmDialog`）。 */
  const [confirmOpen, setConfirmOpen] = useState(false)
  const [running, setRunning] = useState(false)
  /** 当前批次的真实进度。`null` = 还没有第一条回报。 */
  const [progress, setProgress] = useState<TranscodeProgress | null>(null)
  /** 跑完之后的结果。`null` = 还没跑过（或正在跑）。 */
  const [result, setResult] = useState<TranscodeBatch | null>(null)
  const [error, setError] = useState<string | null>(null)
  /** 取消请求已经发出，批次还没停。按钮据此显示「正在停…」。 */
  const [cancelling, setCancelling] = useState(false)
  /** 面板容器。关闭后把焦点还给编辑器。 */
  const panelRef = useRef<HTMLDivElement>(null)
  /**
   * 当前批次的 token。
   *
   * 由前端生成、**单调递增**：取消命令要靠它定位批次，而它必须在
   * 「发起批量」之前就存在——否则从点开始到拿到 token 之间点取消会丢。
   */
  const tokenRef = useRef(0)

  /**
   * 重读统计。
   *
   * `root` 走参数而不是闭包里的 `vault`：这样这个函数是稳定的，
   * `applyVault` 的依赖数组里不会因为它每次重建（`useTrash.refresh` 同款）。
   */
  const refresh = useCallback(async (root: string) => {
    try {
      setStats(await vaultEncodingStats(root))
      setError(null)
    } catch (err) {
      // 统计读不出来不该打断编辑：它是旁路信息，用户没在看它的时候
      // 更不该弹横幅。面板打开时他一定会看见。
      setError(describeError(err))
    }
  }, [])

  /** 换到「没有库」：入口消失。 */
  const reset = useCallback(() => {
    setStats(null)
    setOpen(false)
    setConfirmOpen(false)
    setResult(null)
    setProgress(null)
  }, [])

  /**
   * 索引还在建时继续问，直到它建完。
   *
   * 没有这一段，`stats` 就只在「扫描刚结束」那一刻取一次——而那时后台索引
   * 通常还没建完，`total` 是 0，入口**永远不出现**。schema 升级之后每个
   * 老用户都会撞上这条路（整库重建）。
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
   * 顺序与回收站一致（点了就有反应），重读是为了「上一次关掉之后又转了几篇」
   * 这种情况不显示旧清单。
   */
  const openPanel = useCallback(() => {
    if (!vault) return
    setError(null)
    setResult(null)
    setOpen(true)
    void refresh(vault.root)
  }, [vault, refresh])

  const closePanel = useCallback(() => {
    setOpen(false)
    setConfirmOpen(false)
  }, [])

  const requestStart = useCallback(() => setConfirmOpen(true), [])
  const closeConfirm = useCallback(() => setConfirmOpen(false), [])

  /**
   * 真正开始跑。
   *
   * 清单取自 `stats.nonUtf8`（按路径排序），**不是**重新查一遍：用户在确认框
   * 里看到的就是这份清单，跑之前再查一次等于「你确认的和执行的可能不是同一批」。
   * 两次之间新增的非 UTF-8 篇会在跑完之后的刷新里出现，再点一次就行——
   * 转存是幂等的。
   */
  const start = useCallback(async () => {
    if (!vault || running) return
    const paths = stats?.nonUtf8.map((entry) => entry.path) ?? []
    if (paths.length === 0) return

    const token = tokenRef.current + 1
    tokenRef.current = token

    setConfirmOpen(false)
    setRunning(true)
    setCancelling(false)
    setResult(null)
    setProgress(null)
    setError(null)

    // 先落盘再转存：见 `beforeStart` 的注释。
    await beforeStart()

    const timer = setInterval(() => {
      void noteTranscodeProgress(token)
        .then((snapshot) => {
          // `null` = 这一批已经结束（Rust 侧收尾时移除了进度）。
          // 那不是「归零」，保持最后一格不要往回跳。
          if (snapshot !== null) setProgress(snapshot)
        })
        .catch(() => {
          // 单次轮询失败不值得打断一次 30 秒的操作：下一次 120ms 后会再问。
        })
    }, PROGRESS_POLL_MS)

    try {
      const batch = await noteTranscodeBatch(vault.root, paths, token)
      setResult(batch)
      if (batch.ok.length > 0) await onTranscoded(batch.ok)
      await refresh(vault.root)
    } catch (err) {
      setError(describeError(err))
    } finally {
      clearInterval(timer)
      setRunning(false)
      setCancelling(false)
      setProgress(null)
    }
  }, [vault, running, stats, beforeStart, onTranscoded, refresh])

  /**
   * 请求取消。
   *
   * 粒度是「篇与篇之间」：当前这一篇会跑完（≤40ms），然后停下。
   * 已转好的不回滚——`history/` 里有备份，转存本身幂等。
   */
  const cancel = useCallback(async () => {
    if (!running) return
    setCancelling(true)
    try {
      await noteTranscodeCancel(tokenRef.current)
    } catch (err) {
      // 取消失败最坏的结果是「它继续跑完」，而那是用户本来要做的事。
      // 记下来让面板说清楚，不弹全局横幅。
      setError(describeError(err))
      setCancelling(false)
    }
  }, [running])

  return {
    stats,
    /** 入口是否出现。**只看这个**：0 篇时不许出现「0 篇不是 UTF-8」。 */
    visible: (stats?.total ?? 0) > 0,
    open,
    confirmOpen,
    running,
    progress,
    result,
    error,
    cancelling,
    panelRef,
    refresh,
    reset,
    openPanel,
    closePanel,
    requestStart,
    closeConfirm,
    start,
    cancel,
  }
}
