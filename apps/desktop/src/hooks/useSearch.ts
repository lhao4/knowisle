import { useCallback, useEffect, useRef, useState, type RefObject } from 'react'

import type { ScanResult, SearchHit } from '../api'
import type { NoteEditorHandle } from '../components/NoteEditor'
import { flattenVault, type QuickItem } from '../lib/quick-open'

interface UseSearchOptions {
  /** 有没有打开库。没有库时两个快捷键都空操作。 */
  hasVault: boolean
  /**
   * 回收站面板是否开着。它是全屏遮罩，浮层开在它后面等于没开，
   * 所以两个快捷键都要给它让路。
   */
  trashOpen: boolean
  /**
   * 编辑器句柄。关浮层要把焦点还回去，选中命中要 `revealHit`。
   *
   * 焦点归还走 `editorRef.current?.focus()` 而不是带重试的 `focusEditor`：
   * 浮层关闭时编辑器一定已经在，重试没有意义。
   */
  editorRef: RefObject<NoteEditorHandle | null>
  /** 打开笔记的唯一路径。搜索域只调它，不自己读文件。 */
  openNote: (path: string) => Promise<boolean>
  /** 搜索前的落盘。失败不阻断搜索，见 `prepareSearch`。 */
  flushActiveNote: () => Promise<boolean>
}

/**
 * 搜索域：快速打开（`Mod+P`）与全文搜索（`Mod+Shift+F`）。
 *
 * # 两个浮层为什么在同一个 hook 里
 *
 * 它们不是两个域，是同一件事的两种入口：都"打开一篇笔记"、都要"关掉之后
 * 把焦点还给编辑器"、都互斥（两个快捷键各自挡住对方，也都被回收站挡住）。
 * 拆成两个 hook 会把这三条约束复制两份，而它们必须同时成立才有意义。
 *
 * # `quickItems` 为什么是 ref 而不是 state
 *
 * 它不参与渲染决策——面板打开时读一次就够了。若放进 state，1000 个对象的
 * 数组会让每次 `setState` 都带上一次引用比较，而它真正变化只在切 Vault
 * 那一瞬间。「每次开面板/每次按键都遍历树」是明确禁止的：那会把一次 O(n)
 * 的建表摊到每一次击键上，而结果每次都一样。
 *
 * 归属仍是本 hook：写入方是 `App` 的 `applyVault`（扫描完成），通过
 * `syncCandidates` 这一个口子进来，`App` 不碰 ref 本身。
 */
export function useSearch({
  hasVault,
  trashOpen,
  editorRef,
  openNote,
  flushActiveNote,
}: UseSearchOptions) {
  /** 快速打开浮层是否可见。 */
  const [paletteOpen, setPaletteOpen] = useState(false)
  /** 全文搜索浮层是否可见。 */
  const [searchOpen, setSearchOpen] = useState(false)

  /**
   * 快速打开的全量候选，**只在扫描完成时重建一次**。
   *
   * 放 ref 而不是 state 的理由见本 hook 的头注释。`App.tsx` 在渲染期读
   * `quickItems.current` 是安全的：它只在一处被写（`syncCandidates`），
   * 而它在**同一个函数里**紧挨着 `setVault`——ref 的换代必然伴随一次
   * state 更新，那次更新必然带着 `paletteOpen` 一起提交。
   */
  const quickItems = useRef<QuickItem[]>([])

  /** 扫描结果换代：候选表跟着换。`null` 表示换到了"没有库"。 */
  const syncCandidates = useCallback((result: ScanResult | null) => {
    quickItems.current = result === null ? [] : flattenVault(result.tree)
  }, [])

  /**
   * 快速打开里选中一项：先收浮层，再走同一条打开路径。
   *
   * 顺序不能反——`openNote` 是异步的，若先开笔记再收浮层，浮层会盖着
   * 已经换好的正文停留到 IPC 往返结束，看起来像「点了没反应」。
   */
  const openFromPalette = useCallback(
    (path: string) => {
      setPaletteOpen(false)
      void openNote(path)
    },
    [openNote],
  )

  /**
   * 关闭浮层并把焦点还给编辑器。
   *
   * 光标位置不变是这里的全部重点：`focus()` 只是把 DOM 焦点交还，
   * 不碰 `EditorState.selection`。详见 `NoteEditorHandle.focus`。
   */
  const closePalette = useCallback(() => {
    setPaletteOpen(false)
    editorRef.current?.focus()
  }, [editorRef])

  /**
   * 搜索浮层里选中一条命中：先收浮层，再走同一条打开路径。
   *
   * # 为什么必须 `await`
   *
   * 跨文件时编辑器按 `note.path` 重建。`openNote` resolve 之前，`viewRef`
   * 指向的是即将销毁的旧视图——在它上面 dispatch selection，用户看到的
   * 是「跳过去了，但光标在开头」。`revealHit` 内部还会再兜一层（路径对不上
   * 就等挂载），两道保险各自独立：这一道挡"根本没打开"，那一道挡"打开了但没挂载"。
   */
  const openFromSearch = useCallback(
    async (hit: SearchHit) => {
      setSearchOpen(false)
      const opened = await openNote(hit.path)
      if (opened) editorRef.current?.revealHit(hit)
    },
    [openNote, editorRef],
  )

  /** 关闭搜索浮层并把焦点还给编辑器。光标位置不变——`focus()` 不碰 selection。 */
  const closeSearch = useCallback(() => {
    setSearchOpen(false)
    editorRef.current?.focus()
  }, [editorRef])

  /**
   * 搜索前的落盘。失败不阻断搜索。
   *
   * 不阻断的理由：搜索读的是磁盘（唯一真相源）。冲突时磁盘上是对方那一版，
   * 搜出来的也就是对方那一版——同时冲突横幅已经明示"你的改动还没保存"。
   * 若在这里中断搜索，用户会以为搜索坏了，而真正的问题在保存冲突上。
   */
  const prepareSearch = useCallback(async () => {
    await flushActiveNote()
  }, [flushActiveNote])

  /**
   * `Cmd/Ctrl+P`。
   *
   * # 只注册一份
   *
   * `main.tsx` 开着 `StrictMode`：dev 下每个 effect 会「挂载 → 清理 → 再挂载」。
   * 只要清理函数摘掉的是**同一个函数引用**，稳态下就只剩一份监听。
   * 反过来，把监听写在渲染体里、或用 `onKeyDown` 之外的原生监听却忘了
   * 返回清理函数，一次按键会进来两次——表现是「点一次 Enter 打开两次」，
   * 第二次会把第一次的结果打断。
   *
   * 依赖只有 `hasVault`（布尔量）与两个浮层开关：它们整个会话最多变几次，
   * 所以这个监听不会因为切笔记、打字、保存状态轮询而反复注册。
   *
   * # 只开不关
   *
   * 浮层已经打开时再按一次 `Mod+P` 是**空操作**，不做 toggle。toggle 会
   * 从这条路径关掉浮层，而这条路径拿不到「把焦点还给编辑器」的语义
   * （`closePalette` 才有），于是焦点会落到 body 上——同一次关闭，走 Esc
   * 和走 Mod+P 得到两种焦点状态。关闭只有一个入口：Esc。
   */
  useEffect(() => {
    const onKeyDown = (e: globalThis.KeyboardEvent) => {
      if (e.repeat || e.altKey || e.shiftKey) return
      if (!e.metaKey && !e.ctrlKey) return
      if (e.key !== 'p' && e.key !== 'P') return
      // WebView 里 `Ctrl+P` 是「打印」，不拦会弹出一个打印对话框。
      e.preventDefault()
      // 搜索浮层开着时不响应：两层浮层叠在一起，Esc 该关谁就没有答案了。
      // 回收站同理——它是全屏遮罩，浮层开在它后面等于没开。
      if (!hasVault || searchOpen || trashOpen) return
      setPaletteOpen(true)
    }

    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [hasVault, searchOpen, trashOpen])

  /**
   * `Cmd/Ctrl+Shift+F` 全文搜索。
   *
   * 与 `Mod+P` 是两条独立监听，各自只认自己那个组合键。塞进同一个 handler
   * 会让「按了 F 但没按 Shift」「按了 P 但按了 Shift」这些判断互相纠缠，
   * 而它们本来就是互斥的两种意图。
   *
   * 只开不关：浮层已开时再按是空操作。关闭只有 Esc 一个入口——只有那条路径
   * 带「把焦点还给编辑器」的语义（`closeSearch`）。
   */
  useEffect(() => {
    const onKeyDown = (e: globalThis.KeyboardEvent) => {
      if (e.repeat || e.altKey) return
      if (!e.metaKey && !e.ctrlKey) return
      if (!e.shiftKey) return
      if (e.key !== 'f' && e.key !== 'F') return
      e.preventDefault()
      // 快速打开开着时不响应，理由同上一条（反方向）。
      if (!hasVault || paletteOpen || trashOpen) return
      setSearchOpen(true)
    }

    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [hasVault, paletteOpen, trashOpen])

  return {
    paletteOpen,
    searchOpen,
    quickItems,
    syncCandidates,
    openFromPalette,
    closePalette,
    openFromSearch,
    closeSearch,
    prepareSearch,
  }
}
