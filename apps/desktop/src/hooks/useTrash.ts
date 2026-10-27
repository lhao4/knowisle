import { useCallback, useRef, useState, type Dispatch, type RefObject, type SetStateAction } from 'react'

import {
  describeError,
  trashList,
  trashPurge,
  trashRestore,
  type RestoreResult,
  type ScanResult,
  type TrashEntry,
  type TrashPage,
} from '../api'
import type { NoteEditorHandle } from '../components/NoteEditor'
import { TRASH_MAX_ROWS, TRASH_PAGE_SIZE } from '../lib/trash'
import { parentDir } from './path'

/**
 * 回收站的空状态。
 *
 * 模块级常量而不是每次 `{ entries: [], total: 0, next: null }`：`applyVault(null)`
 * 会把它写回 state，字面量每次都是新对象，会让下游的 `useCallback` 依赖
 * 每次重建。同一个引用也让"没打开过库"和"库是空的"在 React 眼里是一回事。
 */
const EMPTY_TRASH: TrashPage = { entries: [], total: 0, next: null }

interface UseTrashOptions {
  /** 当前库。`null` 时所有动作都是空操作（`openPanel` / `purge` / `restore`）。 */
  vault: ScanResult | null
  /** 关面板要把焦点还给编辑器。 */
  editorRef: RefObject<NoteEditorHandle | null>
  /**
   * 让树露出刚恢复的那一条。
   *
   * `revealPath` 本身是**跨域**的（新建 / 改名 / 删除 / 恢复 / 换库都写它），
   * 所以它留在 `App.tsx`，这里只拿 setter。放进本 hook 会让侧栏域反向
   * import 回收站域，那是把耦合换个地方放。
   */
  setRevealPath: Dispatch<SetStateAction<string | null>>
}

/**
 * 回收站域：列表分页、面板开合、恢复、清空。
 *
 * # 返回的两半是刻意分开的
 *
 * - **面板要的**：`page` / `loading` / `loadingMore` / `restoring` / `purging` /
 *   `error` / `notice` / `purgeOpen` / `panelRef`，以及 `loadMore` / `restore` /
 *   `purge` / `closePurge`。
 * - **侧栏徽标要的**：`page.total`。它只要这一个数字，且**只读**。
 *
 * 不合成一个 `trash` 大对象：那样等于把 1614 行的耦合原样搬进一个 400 行的
 * hook，只是换了个文件而已。
 *
 * # 恢复为什么不整个吞进来
 *
 * 恢复是**跨域编排**：回收站（IPC）→ 库（重扫）→ 编辑器（打开）。后两步要
 * `applyVault` 与 `openNote`，而 `applyVault` 又依赖本 hook 的 `refresh`
 * （扫描完要刷新徽标）。两边互相依赖，硬塞会变成循环。
 *
 * 所以 `restore` 只包「IPC + 三个状态位的开合」，中间那一步由调用方在
 * `after` 回调里做——**时序与原来逐字一致**：`restoring` 覆盖到 `openNote`
 * 结束才清，`error` 在 `finally` 之前写。
 */
export function useTrash({ vault, editorRef, setRevealPath }: UseTrashOptions) {
  /**
   * 回收站列表（**一页 + 总数 + 游标**）。条数的唯一来源：侧栏徽标读
   * `total`，面板读 `entries` 与 `total`。
   *
   * # 为什么不再是一个数组
   *
   * 分页之后「加载了多少」与「一共有多少」必然分叉。拿 `entries.length`
   * 当总数，加载两页之后徽标会写着 400 而回收站里有 10000 条。
   *
   * `next` 由服务端给（多取一条算出来的**事实**），前端只负责原样传回，
   * 绝不拿 `entries.length < total` 自己推——目录被跳过、条目被外部恢复，
   * 都会让"这页不满"而其实还有下一页。
   *
   * 每次扫描、每次删除/清空之后重读**第一页**；恢复一条是唯一的例外：
   * 它只在本地移除那一行（见 `settle`）。
   */
  const [page, setPage] = useState<TrashPage>(EMPTY_TRASH)
  const [open, setOpen] = useState(false)
  const [loading, setLoading] = useState(false)
  /** 「加载更多」在飞。防重复请求：连点两下会拿到同一页，同一批 `entry` 追加两遍。 */
  const [loadingMore, setLoadingMore] = useState(false)
  /** 正在恢复的条目（`entry`）；`null` 表示空闲。 */
  const [restoring, setRestoring] = useState<string | null>(null)
  const [purging, setPurging] = useState(false)
  /** 面板里的错误行：读不出来 / 恢复失败。 */
  const [error, setError] = useState<string | null>(null)
  /**
   * 面板里的**成功**回执（刚恢复了一条）。
   *
   * 与 `error` 分开：恢复成功不是错误，用红色错误行说它会让用户以为
   * 出了事。它也不再走顶栏的 `flash`——面板是模态的，那句话会被盖住。
   * 两条回执**不许合并**：一个在顶栏、一个在面板内，面板开着时顶栏那条看不见。
   */
  const [notice, setNotice] = useState<string | null>(null)
  /** 清空的二次确认是否打开。 */
  const [purgeOpen, setPurgeOpen] = useState(false)
  /** 面板容器。清空确认关掉之后把焦点还回来，理由见 `TrashPanel` 的注释。 */
  const panelRef = useRef<HTMLDivElement>(null)

  /**
   * 重读回收站的**第一页**。
   *
   * 失败只记进面板的错误行，**不弹全局横幅**：回收站读不出来时用户很可能
   * 正在编辑，为一条旁路信息打断他不值得。而面板打开时他一定会看见。
   *
   * 永远从 `cursor = null` 开始：这是"重新看一遍磁盘"的语义，不是"接着上次
   * 翻"。分页状态在 `page` 里，重取会把它整个换掉——恢复一条不走这里
   * （见 `settle`），否则用户会被弹回第 1 页。
   */
  const refresh = useCallback(async (root: string) => {
    setLoading(true)
    try {
      setPage(await trashList(root, null, TRASH_PAGE_SIZE))
      setError(null)
    } catch (err) {
      setError(describeError(err))
    } finally {
      setLoading(false)
    }
  }, [])

  /** 换到"没有库"：列表回到空。`applyVault(null)` 走这里。 */
  const reset = useCallback(() => setPage(EMPTY_TRASH), [])

  /**
   * 加载下一页并追加。
   *
   * # 三条约束，少一条都会出错
   *
   * 1. **只认 `next`**：它为空就是没有下一页。拿 `entries.length < total`
   *    自己推会错——目录被跳过、条目在两次调用之间被恢复，都会让"这页不满"
   *    而其实还有下一页。
   * 2. **防重复请求**：连点两下会拿到同一页并追加两遍（同一个 `entry`
   *    出现两次，React 的 key 冲突，恢复按钮也会指错行）。
   * 3. **1000 行封顶**：那不是分页大小，是**渲染上限**。10k 个 DOM 节点
   *    塞进面板会让滚动掉帧；而翻到第 1000 条还没找到东西时，该做的是搜索
   *    （B-29 的第 ② 步），不是继续翻。
   */
  const loadMore = useCallback(async () => {
    if (!vault) return
    const cursor = page.next
    if (cursor === null || loadingMore) return
    if (page.entries.length >= TRASH_MAX_ROWS) return

    setLoadingMore(true)
    try {
      const fetched = await trashList(vault.root, cursor, TRASH_PAGE_SIZE)
      setPage((current) => {
        // 到上限就只收下放得下的那几条，并把 `next` 清掉——按钮随即变成
        // 「已显示前 1000 条」，不再发请求。
        const room = Math.max(0, TRASH_MAX_ROWS - current.entries.length)
        const appended = fetched.entries.slice(0, room)
        return {
          entries: [...current.entries, ...appended],
          total: fetched.total,
          next: appended.length === fetched.entries.length ? fetched.next : null,
        }
      })
      setError(null)
    } catch (err) {
      setError(describeError(err))
    } finally {
      setLoadingMore(false)
    }
  }, [vault, page.next, page.entries.length, loadingMore])

  /**
   * 打开回收站：**先开面板**，再读列表。
   *
   * 顺序是为了"点了就有反应"：读列表要过一次 IPC，先等它再开面板会有一段
   * 什么都不发生的空白。面板自己会显示「读取中…」，那是诚实的中间态。
   */
  const openPanel = useCallback(() => {
    if (!vault) return
    setError(null)
    setNotice(null)
    setOpen(true)
    void refresh(vault.root)
  }, [vault, refresh])

  /** 关闭回收站并把焦点还给编辑器。取消与恢复成功共用这一条路径。 */
  const closePanel = useCallback(() => {
    setOpen(false)
    setPurgeOpen(false)
    editorRef.current?.focus()
  }, [editorRef])

  /**
   * 从回收站恢复一条的**数据部分**：IPC + 三个状态位。
   *
   * `after` 里放跨域那两步（重扫库、打开笔记）。`restoring` 一直挂到
   * `after` 结束才清——面板上「恢复中」的那一条要等到正文真的换了才放手，
   * 提前清会让按钮闪回可点状态。
   */
  const restore = useCallback(
    async (entry: TrashEntry, after: (result: RestoreResult) => Promise<void>) => {
      if (!vault) return
      setRestoring(entry.entry)
      setError(null)
      setNotice(null)
      try {
        const result = await trashRestore(vault.root, entry.entry)
        await after(result)
      } catch (err) {
        setError(describeError(err))
      } finally {
        setRestoring(null)
      }
    },
    [vault],
  )

  /**
   * 恢复成功之后：本地移除那一行 + 让树露出它 + 写回执。
   *
   * # 为什么**不**整页重取
   *
   * 整页重取会把正在看第 5 页的用户弹回第 1 页，而 keyset 存在的全部理由
   * 就是不让这件事发生。改成本地移除那一行 + `total - 1`——keyset 游标让
   * 已加载的前缀不会移位，所以「加载更多」之后仍然接着下一页，不重不漏。
   * 这是 keyset 与 offset 的分水岭。
   *
   * # 面板**不关**
   *
   * 从回收站里挑东西的人多半还要接着挑下一条。每恢复一条就把面板关掉，
   * 等于让他重新打开、重新翻回原来那一页。正文在面板后面打开，关掉面板
   * （Esc）就能看到它——恢复成功的那句话写在面板的 `notice` 行里，
   * 因为顶栏的 flash 会被面板盖住。
   */
  const settle = useCallback(
    (entry: TrashEntry, result: RestoreResult) => {
      setPage((current) => ({
        entries: current.entries.filter((item) => item.entry !== entry.entry),
        total: Math.max(0, current.total - 1),
        next: current.next,
      }))
      setRevealPath(result.path)
      setNotice(describeRestore(result, entry))
    },
    [setRevealPath],
  )

  /**
   * 清空回收站。**只由确认框触发**，没有任何"顺手清一下"的入口。
   *
   * # 部分失败也关确认框
   *
   * `trash_purge` 永远返回结果而不是错误：删不掉的留在回收目录里，列表刷新
   * 后还看得见。所以这里返回 `null`（关框），把「删了 N 篇、M 篇删不掉」
   * 写进面板的错误行——那行本来就是"整次操作的结果"，而确认框会把它盖住。
   *
   * 提示必须写在 `refresh` **之后**：刷新成功会把错误行清空。
   *
   * 真正的 `Err` 只有一种（回收目录读不出来），那时一个都没动，
   * 让确认框留在屏幕上给用户重试是对的。
   */
  const purge = useCallback(async (): Promise<string | null> => {
    if (!vault) return '还没有打开知识库'
    setPurging(true)
    try {
      const result = await trashPurge(vault.root)
      // 清空之后**重置到第一页**：磁盘上已经空了，留着旧游标只会让「加载更多」
      // 去翻一批不存在的条目。这也是唯一一处该丢掉分页状态的地方。
      await refresh(vault.root)
      setNotice(null)
      if (result.failed > 0) {
        setError(
          `已永久删除 ${result.removed} 篇，${result.failed} 篇删不掉（可能被占用），已留在列表里`,
        )
      }
      return null
    } catch (err) {
      return describeError(err)
    } finally {
      setPurging(false)
    }
  }, [vault, refresh])

  /** 面板上点了「清空」：只发意图，二次确认由上层弹（复用 `ConfirmDialog`）。 */
  const requestPurge = useCallback(() => setPurgeOpen(true), [])

  /**
   * 关掉清空确认，把焦点还给面板。
   *
   * `ConfirmDialog` 卸载后焦点会落到 `<body>`，而 Esc 监听挂在面板容器上——
   * 那一刻用户按 Esc 关不掉回收站，看起来像卡住了。
   */
  const closePurge = useCallback(() => {
    setPurgeOpen(false)
    panelRef.current?.focus()
  }, [])

  return {
    page,
    open,
    loading,
    loadingMore,
    restoring,
    purging,
    error,
    notice,
    purgeOpen,
    panelRef,
    refresh,
    reset,
    loadMore,
    openPanel,
    closePanel,
    restore,
    settle,
    purge,
    requestPurge,
    closePurge,
  }
}

/**
 * 恢复结果的一句话。
 *
 * # 三件事互相独立，必须能同时说
 *
 * ① 为什么没回原处（`fallback`）；② 有没有撞名改名（`renamed`）；
 * ③ 索引更新了没有（`indexed`）。任何两件都能同时发生——`originGone` 落到
 * 库根时，根目录也可能已经有一份同名文件，而那正是最需要说清的时刻：
 * 只说"原目录没了"，用户会以为 `d 2.md` 是别人建的。
 *
 * 所以撞名与索引都是**后缀**，不是分支。早期版本用 `if (degraded) return …`
 * 提前返回，把 `renamed` 整个吞掉了。
 */
function describeRestore(result: RestoreResult, entry: TrashEntry): string {
  // 撞名的措辞要看撞在哪：没降级时撞的是原目录里的名字，降级时撞的是库根里的。
  // 两种都叫"原名已被占用"会让降级那一支听起来像是原处有文件跟他抢名字。
  let renamedNote = ''
  if (result.renamed) {
    renamedNote = result.fallback === null ? '（原名已被占用）' : '，且根目录已有同名文件'
  }
  const indexNote = result.indexed ? '' : '；索引未更新，重启应用会重建'
  return `${describeLanding(result, entry)}${renamedNote}${indexNote}`
}

/**
 * 落点那一句：回原处了、还是降级了、降级到哪。
 *
 * # 为什么拆成独立函数
 *
 * `switch` 的每个分支都 `return`，穷尽性断言才能自然写在末尾。
 * **不写 `default`**：`const _exhaustive: never = result.fallback` 是给编译器
 * 的断言——`Fallback` 将来多一种取值时，这一行会**编译不过**，而不是默默
 * 落进一个"未知情况"的分支。`default` 会让它永远编译得过，那正是这个断言
 * 存在的反面。
 */
function describeLanding(result: RestoreResult, entry: TrashEntry): string {
  switch (result.fallback) {
    case null:
      return `已恢复${result.renamed ? '为' : '到'} ${result.path}`
    case 'recordMissing':
      return `已恢复到库根 ${result.path}：没有这条记录`
    case 'recordCorrupt':
      return `已恢复到库根 ${result.path}：清单记录损坏`
    case 'originGone': {
      // 目录名来自列表数据（`origin` 的父目录）。`originGone` 意味着记录是好的，
      // 所以正常情况下它一定有值；类型上仍是 `null`，兜一句不含路径的话。
      const dir = entry.origin === null ? '' : parentDir(entry.origin)
      return `已恢复到库根 ${result.path}：${dir === '' ? '原目录' : `原目录 ${dir}`} 已不存在`
    }
  }
  const _exhaustive: never = result.fallback
  return _exhaustive
}
