import { useCallback, useEffect, useRef } from 'react'

import { vaultFingerprint, type ScanResult } from '../api'

/**
 * 两次探测之间的最小间隔。
 *
 * 一次聚焦会同时触发 `focus` 与 `visibilitychange`（最小化再还原），
 * 而 `StrictMode` 还会把监听器挂两遍。没有这道闸，一次 Alt+Tab 会探两次。
 */
const PROBE_MIN_GAP_MS = 500

/**
 * 窗口**离开过**多久才值得探一次。
 *
 * ADR-018 门禁①实测（release，21 次采样）：10k 篇 / 100 目录的指纹
 * **p50 = 21.2 ms**（min 18.9 / max 27.0），其中 18.6 ms 是「把 101 个目录
 * readdir 一遍」的底价——本机每个目录约 180 µs。它落在门禁的
 * 「5~20 ms」档的上沿（超出 1.2 ms，在噪声带内），所以取这一档的处方：
 * **只在真的离开过之后探**，而不是每次 focus 都探。
 *
 * 为什么不取「> 20 ms → 停手」那一档：p50 与 20 ms 的差小于采样噪声，
 * 而下一档的处方是 watcher——§5 明确把 ③ 划在另一条路线。这里选**更保守**
 * 的那一档（探得更少），并把数字原样写进 ADR。
 *
 * 副作用是「<1 秒的来回切换不探」。那一段时间里外部改动的概率极低，
 * 而代价是整棵树重新 readdir 一遍——1 秒是这两件事的平衡点。
 */
const AWAY_MIN_MS = 1000

interface TreeProbeArgs {
  /** 当前库。`null` = 还没打开库，什么都不做。 */
  vault: ScanResult | null
  /**
   * 重扫在飞（手动按钮或上一次自动触发）。
   *
   * 它是 `useState`，比 `rescanGate` 慢一帧——但**不需要**在这里挡住
   * 同一帧的并发：`rescanVault` 自己的 ref 闸门才是互斥的唯一真相，
   * 这里只是省一次没必要的指纹计算。
   */
  rescanning: boolean
  /**
   * 有模态 / 面板 / 浮层开着，或正在批量转存。
   *
   * 树在用户眼皮底下换掉是错的：回收站面板正在翻第 2 页、忽略规则面板正在
   * 展示「当前生效的规则」、名字输入框正在等一个回车——这些时刻重扫都会让
   * 他看到的东西与刚读到的说明对不上。命中的探测**记 pending**，关掉之后补一次。
   */
  blocked: boolean
  /**
   * 复用**现有的** `rescanVault`（ADR-017）。返回 `true` 表示扫描成功并已
   * `applyVault`；`false` 表示被闸门挡住或扫描失败。
   *
   * 绝不在这里另写一条扫描路径：那会让「重扫」的语义（不 flush、不重取回收站、
   * 不动 activeNote）出现第二份实现。
   */
  rescan: () => Promise<boolean>
}

/** 一次探测需要的实时状态。放进 ref 是为了让监听器只注册一次。 */
interface Live extends TreeProbeArgs {
  /** 库根。换库时基线必须作废。 */
  root: string | null
}

/**
 * 「树会不会自己刷新」的探针。
 *
 * # 它做什么
 *
 * 窗口重新获得焦点时（且**真的离开过** ≥1s），算一次**目录指纹**——所有目录的
 * 相对路径 + 每个目录下的条目名集合，哈希成一个 hex。与基线不同就调
 * `rescanVault()` 让树与磁盘对齐。
 *
 * # 它不做什么
 *
 * - **不管内容**：编辑器里那一篇有没有被外部改过，归 `revalidate`（聚焦校验）。
 *   两者刻意不互相调用：形状是整库的，内容是当前这一篇的。
 * - **不碰编辑器**：不 flush、不动 `activeNote` 的 path / docId、不重建
 *   `EditorState`。自动重扫与手动重扫走同一条 `rescanVault`，继承 ADR-017 的
 *   全部约束。
 * - **不写盘**：指纹只 readdir，基线只在内存。
 *
 * # 三条不能改的
 *
 * 1. **基线只活在内存**（`useRef`）。写进 `localStorage` 会让「上次那台机器上
 *    的形状」冒充「这一次的形状」——重启后第一次聚焦必然误判，而误判的代价是
 *    一次全量重扫。
 * 2. **失败不刷基线**。库目录临时读不出来时刷了基线，恢复之后就再也探不到了：
 *    基线记的是「读不到」，而磁盘上的真实形状永远不会等于它。
 * 3. **扫描成功之后必须重建基线**（见下面那个 effect）。少了这一步，自动重扫
 *    会把「自己刚扫完的新形状」当成「又变了」，于是每次聚焦都重扫一次。
 */
export function useTreeProbe({ vault, rescanning, blocked, rescan }: TreeProbeArgs) {
  /** 上次扫描完成时的形状。`null` = 还没有可信基线。 */
  const baseline = useRef<{ root: string; fp: string } | null>(null)
  /** 正在算基线。这期间不探——否则会拿旧基线比新形状。 */
  const priming = useRef(false)
  /** 正在探。一次聚焦只发一次指纹 IPC。 */
  const probing = useRef(false)
  /** 被避让挡下的那一次，等门开了补。 */
  const pending = useRef(false)
  const lastProbeAt = useRef(0)
  /** 窗口是什么时候离开的（`Date.now()`）。`null` = 一直在前台。 */
  const awaySince = useRef<number | null>(null)
  /** 已经建过基线的那个 `vault` 对象。StrictMode 下 effect 跑两次，靠它去重。 */
  const primed = useRef<ScanResult | null>(null)
  /**
   * 第几次算基线。**换库时会有两次 prime 叠在一起**（旧库那次还没回来，
   * 新库这次已经出发），只有序号最大的那次能写基线——否则旧库的指纹会
   * 盖掉新库的，下一次聚焦立刻误判「变了」。
   */
  const primeSeq = useRef(0)

  const live = useRef<Live>({ vault, rescanning, blocked, rescan, root: vault?.root ?? null })
  // 渲染之后同步一次：监听器只注册一次，读到的必须是这一帧的值。
  // 与 `App.tsx` 里 `checkRef` 的写法逐字相同（那边有一整段解释）。
  useEffect(() => {
    live.current = { vault, rescanning, blocked, rescan, root: vault?.root ?? null }
  })

  /**
   * 算一次指纹并记为基线。**只在扫描成功之后调**（或基线不可信时重建）。
   */
  const prime = useCallback(async (root: string) => {
    const seq = ++primeSeq.current
    priming.current = true
    try {
      const fp = await vaultFingerprint(root)
      // 期间又换了一次库：这一份属于上一个库，丢掉。
      if (seq === primeSeq.current) baseline.current = { root, fp }
    } catch {
      // 读不出来就**不刷**基线：留着旧的那份（或 null），下一次聚焦会再试。
      // 这里刻意不 `baseline.current = null`：那会让下一次探测走「没有基线」
      // 分支而白算一次，却仍然解决不了问题。
    } finally {
      // 只有最后一次 prime 有权把闸门放开——前面那些的 finally 先跑，
      // 放开之后 probe 会拿着半旧的基线比，正好比出「变了」。
      if (seq === primeSeq.current) priming.current = false
    }
  }, [])

  /**
   * 探一次。由 `App.tsx` 那个**唯一的** focus / visibilitychange 监听调用。
   *
   * 顺序不能换：避让 → 节流 → 取指纹 → 比基线 → 重扫。
   * 反过来的话，被避让挡下的那一次会先花掉一次 readdir。
   */
  const probe = useCallback(async () => {
    const now = Date.now()
    const state = live.current
    if (state.root === null) return
    if (state.rescanning) return
    if (state.blocked) {
      // 记下来，等门开了补一次（见下面那个 effect）。**不**在这里硬闯。
      pending.current = true
      return
    }
    if (priming.current || probing.current) return
    if (now - lastProbeAt.current < PROBE_MIN_GAP_MS) return
    lastProbeAt.current = now

    probing.current = true
    try {
      const fp = await vaultFingerprint(state.root)
      // 算的这几毫秒里用户可能换了库（`pickVault`）。换库之后这份指纹属于
      // 上一个库，拿它去比新库、或者用它触发一次重扫，都会把旧库的树装回来。
      if (live.current.root !== state.root) return
      const known = baseline.current
      // 没有可信基线（首次扫描还没回来 / 上一次算失败了 / 换库了）：
      // 记下来就好。**绝不**在没有基线时重扫——那会把「不知道」当成「变了」。
      if (known === null || known.root !== state.root) {
        baseline.current = { root: state.root, fp }
        return
      }
      if (fp === known.fp) return

      // 失败不刷基线：`rescan` 返回 false 时基线留着旧值，下一次聚焦还会再试。
      // 成功时 `vault` 会换对象 → 下面那个 effect 重建基线。
      await state.rescan()
    } catch {
      // 指纹算不出来（库目录被删 / 没权限）：什么都不做，下一次再试。
    } finally {
      probing.current = false
    }
  }, [])

  /**
   * 窗口离开。`awaySince` 只记第一次：最小化会同时触发 `blur` 与
   * `visibilitychange(hidden)`，记第二次等于把「离开时长」清零。
   */
  const onBlur = useCallback(() => {
    if (awaySince.current === null) awaySince.current = Date.now()
  }, [])

  /**
   * 窗口回来。**这里是 1 秒规则的唯一落点**（理由见 `AWAY_MIN_MS`）。
   *
   * `awaySince === null` 时照探：那说明我们没看见离开（首次聚焦、或被别的
   * 窗口盖住又回来而没派发 blur）。不知道离开了多久，就按「值得探」处理——
   * 漏探比多探一次贵得多（漏探 = 树不刷新，那是这一刀要修的病）。
   */
  const onFocus = useCallback(() => {
    const away = awaySince.current
    awaySince.current = null
    if (away !== null && Date.now() - away < AWAY_MIN_MS) return
    void probe()
  }, [probe])

  /**
   * 每次扫描完成（`vault` 换了对象）立即重建基线。
   *
   * 这条 effect 是「自动重扫不会变成死循环」的全部保证：没有它，重扫之后基线
   * 还是旧的，下一次聚焦立刻又判「变了」。
   *
   * 它同时覆盖**用户自己**的新建 / 改名 / 删除 / 恢复——那些动作都会
   * `applyVault`，于是基线跟着前进，下一次聚焦不会把用户自己的操作当成外部改动。
   *
   * 代价是每次 `applyVault` 多一次 readdir（10k / 100 目录约 21 ms）。可接受：
   * 它发生在一次整库扫描之后，那本来就是几十到几百毫秒的动作。
   */
  useEffect(() => {
    if (vault === null) {
      baseline.current = null
      primed.current = null
      return
    }
    if (primed.current === vault) return
    primed.current = vault
    void prime(vault.root)
  }, [vault, prime])

  /**
   * 避让解除之后补一次。
   *
   * 判据 7：回收站面板开着时切回 → 不重扫；关掉面板之后补一次。
   * 只认 `blocked`：`rescanning` 变 false 时**不补**——那一次重扫刚刚扫完，
   * 基线已经跟着新 `vault` 重建了，再探一次只会白读一遍目录。
   */
  useEffect(() => {
    if (blocked || rescanning) return
    if (!pending.current) return
    pending.current = false
    void probe()
  }, [blocked, rescanning, probe])

  return { onFocus, onBlur }
}
