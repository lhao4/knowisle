import { useCallback, useEffect, useRef, useState } from 'react'

import { ConfirmDialog } from './components/ConfirmDialog'
import { ContextMenu, type ContextMenuItem } from './components/ContextMenu'
import { TreeNode } from './components/FileTree'
import { NamePrompt } from './components/NamePrompt'
import { NoteEditor, type NoteEditorHandle } from './components/NoteEditor'
import { Outline, type OutlineHandle } from './components/Outline'
import { QuickOpen } from './components/QuickOpen'
import { SearchOverlay } from './components/SearchOverlay'
import type { OutlineItem } from './lib/outline'
import { flattenVault, type QuickItem } from './lib/quick-open'
import {
  appPing,
  describeError,
  noteCreate,
  noteDelete,
  noteEdit,
  noteFlush,
  noteRead,
  noteRename,
  noteStatus,
  pickFolder,
  vaultScan,
  type FileNode,
  type NoteContent,
  type SaveState,
  type ScanResult,
  type SearchHit,
} from './api'

/**
 * 前端多久查一次保存状态。
 *
 * 只在 `dirty` 为真时轮询——自动落盘发生在 Rust 侧，前端没有别的办法
 * 知道它什么时候完成。空闲时完全不轮询，所以稳态下开销是零。
 */
const STATUS_POLL_MS = 500

/**
 * 手动保存（`Mod-s`）后「已保存」在标题栏停留多久。
 *
 * 它是一条**一次性回执**，不是状态：轮询出来的 `saveState` 要等 500ms 才更新，
 * 而按键的反馈必须是即时的。两者分开，谁也不等谁。
 */
const SAVED_FLASH_MS = 1200

/** 上次打开的 Vault 绝对路径。 */
const LAST_VAULT_KEY = 'knowisle.lastVault'

/**
 * 正在输入名字的浮层。
 *
 * 两种意图共用一个组件，但**位置来源不同**：
 * - `create` 只需要目录（当前笔记所在目录，没有选中就是根目录）；
 * - `rename` 需要旧路径（新名字只在同一个目录内生效）。
 */
type PromptState =
  | { kind: 'create'; dir: string }
  | { kind: 'rename'; path: string; name: string }

/** 右键菜单：位置 + 它属于哪个节点（节点只用来生成菜单项，用完即弃）。 */
interface MenuState {
  node: FileNode
  x: number
  y: number
}

/** 等待二次确认的删除目标。 */
interface DeleteTarget {
  path: string
  name: string
}

/** 取 Vault 相对路径的父目录（POSIX 分隔符）。根目录返回空串。 */
function parentDir(path: string): string {
  const index = path.lastIndexOf('/')
  return index < 0 ? '' : path.slice(0, index)
}

export default function App() {
  const [vault, setVault] = useState<ScanResult | null>(null)
  const [activeNote, setActiveNote] = useState<NoteContent | null>(null)
  /**
   * 当前文档的**身份**。打开新笔记时自增，重命名时不变。
   *
   * 编辑器重建只认它（见 `NoteEditor` 的 `docId`）。改名不重建，所以撤销栈、
   * 光标、滚动位置都留在原地——这三个东西属于"这篇文档"，不属于"这个路径"。
   */
  const [docId, setDocId] = useState(0)
  const [saveState, setSaveState] = useState<SaveState | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [runtime, setRuntime] = useState<string>('')
  /** 手动保存的回执，`null` 表示没有正在显示的提示。 */
  const [flash, setFlash] = useState<string | null>(null)
  const flashTimer = useRef<number | null>(null)
  const editorRef = useRef<NoteEditorHandle>(null)
  /** 正在输入名字的浮层。`null` 表示没有。 */
  const [prompt, setPrompt] = useState<PromptState | null>(null)
  /** 右键菜单。 */
  const [menu, setMenu] = useState<MenuState | null>(null)
  /** 等待二次确认的删除目标。`null` 表示没有确认框。 */
  const [deleteTarget, setDeleteTarget] = useState<DeleteTarget | null>(null)
  /**
   * 需要在树里露出来的路径（新建 / 改名之后）。
   *
   * 只在**写入那一刻**设一次：`TreeNode` 按"值变化"展开目录，所以用户随后
   * 手动折叠不会被下一次无关重渲染弹开。
   */
  const [revealPath, setRevealPath] = useState<string | null>(null)
  /**
   * 大纲的标题集合。**低频**：只有标题增删改时才变。
   *
   * 当前章节（高亮）刻意不在这里——它每次光标移动都变，走 `outlineRef`
   * 的句柄直接改 DOM。见 `OutlineHandle` 的注释。
   */
  const [outline, setOutline] = useState<OutlineItem[]>([])
  const outlineRef = useRef<OutlineHandle>(null)
  /** 快速打开浮层是否可见。 */
  const [paletteOpen, setPaletteOpen] = useState(false)
  /** 全文搜索浮层是否可见。 */
  const [searchOpen, setSearchOpen] = useState(false)
  /**
   * 快速打开的全量候选，**只在扫描完成时重建一次**。
   *
   * 放 ref 而不是 state：它不参与渲染决策——面板打开时读一次就够了。
   * 若放进 state，1000 个对象的数组会让每次 setState 都带上一次引用比较，
   * 而它真正变化只在切 Vault 那一瞬间。
   *
   * 「每次开面板/每次按键都遍历树」是明确禁止的：那会把一次 O(n) 的
   * 建表摊到每一次击键上，而结果每次都一样。
   */
  const quickItems = useRef<QuickItem[]>([])

  /**
   * 安装一次扫描结果：树进 state，扁平候选表进 ref。
   *
   * 两件事必须**一起**做。分开写（某处 setVault、另一处忘了重建候选表）
   * 会让面板拿着上一个库的路径去 `noteRead`——报错是 PATH_ESCAPE，
   * 而真正的原因在另一个文件里。收成一个函数就没有漏掉的机会。
   */
  const applyVault = useCallback((result: ScanResult | null) => {
    setVault(result)
    quickItems.current = result === null ? [] : flattenVault(result.tree)
  }, [])

  const showFlash = useCallback((text: string) => {
    if (flashTimer.current !== null) window.clearTimeout(flashTimer.current)
    setFlash(text)
    flashTimer.current = window.setTimeout(() => {
      flashTimer.current = null
      setFlash(null)
    }, SAVED_FLASH_MS)
  }, [])

  // 组件卸载时把定时器收掉，否则它会对着已卸载的组件 setState。
  useEffect(
    () => () => {
      if (flashTimer.current !== null) window.clearTimeout(flashTimer.current)
    },
    [],
  )

  // 启动时 ping 一次：确认 Rust 侧真的起来了。
  // 如果这一句失败，后面所有按钮都会失败，报错信息会误导排查方向。
  useEffect(() => {
    appPing()
      .then((info) => setRuntime(`v${info.version} · pid ${info.pid}`))
      .catch((err) => setRuntime(`IPC 未就绪：${describeError(err)}`))
  }, [])

  // 恢复上次打开的 Vault。失败就静默清掉记录——目录可能被删了、改名了、
  // 或者挪到了没权限的地方，这些都不该在启动时糊用户一脸错误。
  useEffect(() => {
    const last = localStorage.getItem(LAST_VAULT_KEY)
    if (last === null) return
    vaultScan(last)
      .then(applyVault)
      .catch(() => localStorage.removeItem(LAST_VAULT_KEY))
  }, [applyVault])

  const pickVault = useCallback(async () => {
    setError(null)
    try {
      const selected = await pickFolder('选择知识库文件夹')
      // 用户点了取消：返回 null，不是错误
      if (selected === null) return

      setBusy(true)
      const result = await vaultScan(selected)
      applyVault(result)
      setActiveNote(null)
      setSaveState(null)
      setOutline([])
      // 换库：上一轮的"露出来"和浮层都不再指向任何东西。
      setRevealPath(null)
      setPrompt(null)
      setMenu(null)
      localStorage.setItem(LAST_VAULT_KEY, result.root)
    } catch (err) {
      applyVault(null)
      setActiveNote(null)
      setOutline([])
      setError(describeError(err))
    } finally {
      setBusy(false)
    }
  }, [applyVault])

  /**
   * 在途的冲刷。
   *
   * # 为什么必须串行化
   *
   * 两次 `noteEdit` 撞在一起时，第二次带的是**上一次落盘之前**的基线
   * （基线要等第一次写完才推进），于是第二次写盘必然 `CONFLICT`——
   * 而这两次写的其实是同一份内容，用户看到的是「我刚打了个字，保存就冲突了」。
   *
   * 这个窗口在开发模式下尤其容易撞：`StrictMode` 会把 effect 挂载两次，
   * 搜索浮层的 `onPrepare` 于是被调用两次，两次冲刷几乎同时出发。
   * 挡在调用方（而不是改 autosave 的写入链路）是刻意的：写入的 1.5s 节拍、
   * 原子写与乐观锁都不该为了一个 UI 时序问题而改动。
   */
  const flushInFlight = useRef<Promise<boolean> | null>(null)

  /**
   * 把编辑器里的内容推给 Rust 并立刻落盘。
   *
   * 只做「先取内容 → noteEdit → noteFlush」这一条顺序——它就是唯一的落盘路径，
   * `openNote` 与「搜索前冲刷」都走它。反过来（先 flush 再取内容）会漏掉
   * 300ms 合并窗口里还没发出去的字符：`noteFlush` 只冲刷 Rust 槽位，
   * 槽位里没有的那几个字它当然写不出来。
   *
   * 返回 `false` 表示失败（典型是冲突）。此时磁盘上没有最新内容，
   * 由调用方决定"留在原地"还是"照常继续"：切换笔记必须留，搜索要继续。
   */
  const flushActiveNote = useCallback(async (): Promise<boolean> => {
    const inFlight = flushInFlight.current
    if (inFlight !== null) return inFlight
    if (!vault || !activeNote) return true

    const task = (async () => {
      const content = editorRef.current?.getContent()
      try {
        if (content !== undefined) {
          await noteEdit(vault.root, activeNote.path, content, activeNote.mtime)
          // 内容已经完整交给 Rust（紧接着 noteFlush 会把它写下去），
          // 把编辑器里那个 300ms 合并窗口清掉。不清的话它会到点重发同一份
          // 内容——平时只是浪费一次 IPC，**改名之后那一次会写到旧路径上**，
          // 于是磁盘上多出一个"改名前的旧文件"。
          editorRef.current?.commitFlushed(content)
        }
        await noteFlush(vault.root, activeNote.path, false)
        return true
      } catch (err) {
        setError(describeError(err))
        setSaveState(await noteStatus(vault.root, activeNote.path).catch(() => null))
        return false
      }
    })()

    flushInFlight.current = task
    try {
      return await task
    } finally {
      flushInFlight.current = null
    }
  }, [vault, activeNote])

  /**
   * 打开一篇笔记（按 Vault 相对路径）。
   *
   * 树点击、快速打开、全文搜索共用这一条路径——**绝不新写一套**。三个入口
   * 各自实现一遍「先 flush 再切换」，早晚会有一个忘了 flush，而那种丢失是静默的。
   *
   * # 顺序不能换
   *
   * 编辑器的 IPC 合并窗口是 300ms，里面可能还有没发出去的字符。
   * 所以必须「先取内容 → 发给 Rust → flush → 才切换」。
   * 反过来做（先切再 flush）会把最后那几个字丢掉，而且丢得毫无痕迹。
   *
   * # flush 失败就不切
   *
   * 典型失败是冲突（文件被别的程序改过）。此时切走等于把这篇的修改丢了，
   * 所以留在原地，让用户处理冲突横幅。
   *
   * # 返回值
   *
   * 调用方要的其实是「切换到底发生了没有」：搜索跳转必须在 `true` 之后
   * 才 dispatch selection，否则会打到一篇根本没打开的笔记上。
   */
  const openNote = useCallback(
    async (path: string): Promise<boolean> => {
      if (!vault) return false
      if (activeNote?.path === path) return true

      if (activeNote && !(await flushActiveNote())) return false

      try {
        const note = await noteRead(vault.root, path)
        // 先清空再换文档：编辑器重建后会在挂载时把新大纲发上来，中间这一帧
        // 如果留着上一篇的标题，用户会看到「标题还是上一篇的」闪一下。
        setOutline([])
        // 新文档 = 新身份。编辑器据此重建，撤销栈归零（那本来就是另一篇）。
        setDocId((id) => id + 1)
        setActiveNote(note)
        setSaveState(null)
        setError(null)
        return true
      } catch (err) {
        setError(describeError(err))
        return false
      }
    },
    [vault, activeNote, flushActiveNote],
  )

  /** 树节点点击。`FileNode` 到路径的转换只在这里发生。 */
  const selectNote = useCallback(
    (node: FileNode) => {
      void openNote(node.path)
    },
    [openNote],
  )

  /**
   * 把焦点还给编辑器。
   *
   * # 为什么不是「等一帧」
   *
   * 新建之后编辑器是**刚挂载**的，而 CM6 的 `EditorView` 是在挂载 effect 里
   * 创建的——它可能晚于下一帧的 rAF 回调。那一刻 `focus()` 是空操作（视图还
   * 不存在），用户看到的是「输入框没了，但光标也不在正文里」。这是时序问题，
   * 不是每次都发生（本机实测约三次里一次），所以只能按帧重试。
   *
   * # 判据为什么是 activeElement
   *
   * 视图不存在时 `focus()` 与「已经聚焦」无法区分（都是无声返回），而
   * `document.activeElement` 是浏览器给出的既成事实。要求**连续两帧**确认，
   * 是为了排掉「聚焦在即将被销毁的旧视图上」的那一帧：换文档时旧视图的 DOM
   * 会连同焦点一起消失，只看一帧会把那一帧当成成功。
   *
   * # 别人抢走焦点就立刻收手
   *
   * 重试期间用户可能已经点开了另一个浮层。若继续抢，浮层输入框会在打开后的
   * 一两帧里被夺走焦点——表现是「打字没反应」，比不给焦点更糟。这里用
   * `focusin` 判断而不是每帧比对：轮询永远慢一帧，而抢焦点慢一帧就够难看了。
   *
   * 上限 12 帧（约 200ms）。超时同样放弃——那时用户已经在干别的了。
   */
  const focusEditor = useCallback(() => {
    let left = 12
    let confirmed = 0

    /** 焦点落到别处（新开的浮层、被点的按钮）就作废这一轮。 */
    const onElsewhere = (e: FocusEvent) => {
      const el = e.target
      if (el instanceof Element && el.classList.contains('cm-content')) return
      stop()
    }

    const stop = () => {
      left = 0
      document.removeEventListener('focusin', onElsewhere, true)
    }

    document.addEventListener('focusin', onElsewhere, true)

    const tick = () => {
      if (left <= 0) return
      editorRef.current?.focus()
      const landed = document.activeElement?.classList.contains('cm-content') === true
      confirmed = landed ? confirmed + 1 : 0
      if (confirmed >= 2) return stop()
      left -= 1
      if (left <= 0) return stop()
      window.requestAnimationFrame(tick)
    }
    window.requestAnimationFrame(tick)
  }, [])

  /** 关闭名字浮层并把焦点还给编辑器。取消与成功共用这一条路径。 */
  const closePrompt = useCallback(() => {
    setPrompt(null)
    focusEditor()
  }, [focusEditor])

  /**
   * 新建笔记。`dir` 是目标目录（Vault 相对路径，空串表示根目录）。
   *
   * # 顺序
   *
   * 先把当前笔记写完 → 建文件 → **整库重扫** → 打开新笔记。
   *
   * 重扫在打开之前：树必须先有这一项，`openNote` 之后的高亮才有落点。
   * 反过来也能用（`openNote` 不依赖树），但用户会看到"正文换了、侧栏还是旧的"
   * 那一帧。
   *
   * # 为什么整库重扫
   *
   * 磁盘是唯一真相源。增量插一个节点看着更快，但它等于在内存里维护第二份
   * 树，一旦某次判断错了（比如名字大小写在 Windows 上折叠了），错误会一直
   * 留在界面上，直到下次手动扫描。1000 篇的整库扫描是 15ms 量级，
   * 不值得为它发明一套 diff。
   */
  const submitCreate = useCallback(
    async (dir: string, name: string): Promise<string | null> => {
      if (!vault) return '还没有打开知识库'
      // 新建会切到一篇新文档，而切换的前提是上一篇已经落盘。
      if (!(await flushActiveNote())) return '当前笔记没有落盘，请先处理保存冲突'
      try {
        const path = await noteCreate(vault.root, dir, name)
        applyVault(await vaultScan(vault.root))
        setRevealPath(path)
        if (!(await openNote(path))) return '笔记已创建，但打开失败'
        return null
      } catch (err) {
        return describeError(err)
      }
    },
    [vault, flushActiveNote, applyVault, openNote],
  )

  /**
   * 重命名笔记（同目录内）。
   *
   * # 为什么不用重建编辑器
   *
   * 改名之后编辑器仍然编辑**同一篇文档**：撤销栈、光标、滚动位置都该留着。
   * 所以这里只换 `activeNote.path`，不动 `docId`——`NoteEditor` 的挂载依赖
   * 是 docId，path 只是它的 IPC 参数。
   *
   * # 先 flush 是必须的，但不够
   *
   * 前端 flush 把 300ms 合并窗口里的字交出去；Rust 侧还要 drain 一遍自动保存
   * 的 worker（它可能正握着更早的一份 pending）。两件事都在 `note_rename`
   * 里串好了，前端不能只做一半。
   */
  const submitRename = useCallback(
    async (path: string, name: string): Promise<string | null> => {
      if (!vault) return '还没有打开知识库'
      const isActive = activeNote?.path === path
      if (isActive && !(await flushActiveNote())) {
        return '当前笔记没有落盘，请先处理保存冲突'
      }
      try {
        const next = await noteRename(vault.root, path, name)
        // 同名：Rust 侧是空操作，这里也不必重扫。
        if (next === path) return null
        applyVault(await vaultScan(vault.root))
        setRevealPath(next)
        if (isActive) {
          setActiveNote((prev) => (prev === null ? prev : { ...prev, path: next }))
        }
        return null
      } catch (err) {
        return describeError(err)
      }
    },
    [vault, activeNote, flushActiveNote, applyVault],
  )

  /** 右键菜单：坐标来自事件，节点只用来决定菜单项。 */
  const openMenu = useCallback((node: FileNode, x: number, y: number) => {
    setMenu({ node, x, y })
  }, [])

  const closeMenu = useCallback(() => setMenu(null), [])

  /**
   * 删除笔记：移进 `.knowisle/trash/`，然后整库重扫。
   *
   * # 顺序（每一步都不能换）
   *
   * 1. **先 flush 当前笔记**（如果删的就是它）。槽位里可能压着最后 1.5 秒的
   *    输入，不落盘就搬目录项，trash 里那份是**旧版本**——删除变成了
   *    "悄悄回退一个版本再删"。失败（冲突）就直接放弃：此刻用户还有
   *    「覆盖 / 放弃」两个选择，删掉就只剩一个。
   * 2. `noteDelete`（Rust 侧还会再 drain 一遍，并清空槽位）。
   * 3. **先 `discardPending` 再 `setActiveNote(null)`**。反了的话编辑器先卸载，
   *    卸载的 cleanup 会把缓冲里的内容再 flush 一次——落在已删除的路径上。
   *    清空缓冲之后，cleanup 里的判断自然为假。
   * 4. 重扫。磁盘是唯一真相源，删完必须重新看一遍。
   *
   * # `discardPending` 只在删的是当前文档时调
   *
   * `pendingRef` 属于**当前**编辑器。删别的笔记时清它，等于把当前笔记
   * 最后 300ms 的字悄悄扔掉——那是另一种数据丢失。
   */
  const submitDelete = useCallback(
    async (path: string): Promise<string | null> => {
      if (!vault) return '还没有打开知识库'
      const isActive = activeNote?.path === path
      if (isActive && !(await flushActiveNote())) {
        return '当前笔记没有落盘，请先处理保存冲突'
      }

      try {
        await noteDelete(vault.root, path)
      } catch (err) {
        return describeError(err)
      }

      // 到这里文件已经在 trash 里了。下面每一步都只清内存，不再碰磁盘。
      if (isActive) {
        editorRef.current?.discardPending()
        setActiveNote(null)
        setOutline([])
        setSaveState(null)
      }
      // `revealPath` 可能正指着刚删掉的那条路径，留着会让树多展开一次
      // 已经不存在的目录。
      setRevealPath(null)

      try {
        applyVault(await vaultScan(vault.root))
      } catch (err) {
        // 删除已经成功，重扫失败不该把确认框留在屏幕上让用户再点一次
        // （再点会报"文件不存在"）。错误照常展示。
        setError(describeError(err))
      }
      return null
    },
    [vault, activeNote, flushActiveNote, applyVault],
  )

  /**
   * 关闭确认框并把焦点还给编辑器。
   *
   * 删除成功后编辑器已经卸载，`focus()` 是空操作（句柄里的 `viewRef` 为 null），
   * 所以这条路径不需要分支——取消与成功共用它。
   */
  const closeDelete = useCallback(() => {
    setDeleteTarget(null)
    focusEditor()
  }, [focusEditor])

  /**
   * 菜单项。目录只有「新建」，文件多「重命名」与「删除」。
   *
   * 目录不改名也不删除：那会把一整片笔记从树里搬走（扫描器按路径建树），
   * 属于"移动"而不是"改名"，本轮明确不做。菜单里不出现，用户就不会去试。
   */
  const menuItemsFor = (node: FileNode): ContextMenuItem[] => {
    const dir = node.kind === 'dir' ? node.path : parentDir(node.path)
    const items: ContextMenuItem[] = [
      { label: '新建笔记', onSelect: () => setPrompt({ kind: 'create', dir }) },
    ]
    if (node.kind === 'file') {
      items.push({
        label: '重命名',
        onSelect: () => setPrompt({ kind: 'rename', path: node.path, name: node.name }),
      })
      items.push({
        label: '删除',
        onSelect: () => setDeleteTarget({ path: node.path, name: node.name }),
      })
    }
    return items
  }

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
  }, [])

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
    [openNote],
  )

  /** 关闭搜索浮层并把焦点还给编辑器。光标位置不变——`focus()` 不碰 selection。 */
  const closeSearch = useCallback(() => {
    setSearchOpen(false)
    editorRef.current?.focus()
  }, [])

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
   * 依赖只有 `hasVault`（布尔量）：它整个会话最多变一次，所以这个监听
   * 不会因为切笔记、打字、保存状态轮询而反复注册。
   *
   * # 只开不关
   *
   * 浮层已经打开时再按一次 `Mod+P` 是**空操作**，不做 toggle。toggle 会
   * 从这条路径关掉浮层，而这条路径拿不到「把焦点还给编辑器」的语义
   * （`closePalette` 才有），于是焦点会落到 body 上——同一次关闭，走 Esc
   * 和走 Mod+P 得到两种焦点状态。关闭只有一个入口：Esc。
   */
  const hasVault = vault !== null
  useEffect(() => {
    const onKeyDown = (e: globalThis.KeyboardEvent) => {
      if (e.repeat || e.altKey || e.shiftKey) return
      if (!e.metaKey && !e.ctrlKey) return
      if (e.key !== 'p' && e.key !== 'P') return
      // WebView 里 `Ctrl+P` 是「打印」，不拦会弹出一个打印对话框。
      e.preventDefault()
      // 搜索浮层开着时不响应：两层浮层叠在一起，Esc 该关谁就没有答案了。
      if (!hasVault || searchOpen) return
      setPaletteOpen(true)
    }

    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [hasVault, searchOpen])

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
      if (!hasVault || paletteOpen) return
      setSearchOpen(true)
    }

    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [hasVault, paletteOpen])

  /** 大纲点击：把光标送到该标题行，并把这一行滚到视口顶部。 */
  const jumpTo = useCallback((pos: number) => {
    editorRef.current?.jumpTo(pos)
  }, [])

  // 有内容没落盘时才轮询。依赖 `dirty` 而不是整个 `saveState`，
  // 否则每次状态更新都会重建 interval，等于没有轮询。
  const dirty = saveState?.dirty === true
  useEffect(() => {
    if (!vault || !activeNote || !dirty) return
    const timer = window.setInterval(() => {
      noteStatus(vault.root, activeNote.path)
        .then(setSaveState)
        .catch(() => {
          // 轮询失败不打断编辑，下一次还会试。
        })
    }, STATUS_POLL_MS)
    return () => window.clearInterval(timer)
  }, [vault, activeNote, dirty])

  /** 冲突时用户选择「用我的版本覆盖」：跳过乐观锁，直接写。 */
  const forceSave = useCallback(async () => {
    if (!vault || !activeNote) return
    try {
      await noteFlush(vault.root, activeNote.path, true)
      setSaveState(await noteStatus(vault.root, activeNote.path))
      setError(null)
    } catch (err) {
      setError(describeError(err))
    }
  }, [vault, activeNote])

  const conflict = saveState?.lastError?.code === 'CONFLICT'
  const saveLabel = flash ?? describeSaveState(saveState)

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="brand__mark">◈</span>
          <span className="brand__name">知屿 KnowIsle</span>
          <span className="brand__tag">M1</span>
        </div>

        <div className="topbar__actions">
          {vault && (
            <span className="stats" title={vault.root}>
              {vault.totalFiles} 篇 · {vault.totalDirs} 个目录 · {vault.elapsedMs} ms
              {vault.skipped > 0 && ` · 跳过 ${vault.skipped}`}
            </span>
          )}
          <button type="button" className="btn" onClick={pickVault} disabled={busy}>
            {busy ? '扫描中…' : '选择文件夹'}
          </button>
        </div>
      </header>

      {error && (
        <div className="banner banner--error" role="alert">
          {error}
        </div>
      )}

      {conflict && (
        <div className="banner banner--warn" role="alert">
          <span>
            这篇笔记已被其它程序修改，你的改动还没有保存到磁盘。「覆盖」会丢弃对方的版本。
          </span>
          <button type="button" className="btn btn--small" onClick={forceSave}>
            用我的版本覆盖
          </button>
        </div>
      )}

      {vault?.truncated && (
        <div className="banner banner--warn" role="status">
          文件数超过上限（20000），已截断。当前只索引了前 20000 个条目。
        </div>
      )}

      <main className="workspace">
        <aside className="sidebar">
          {vault ? (
            <>
              {/*
                侧栏顶部的「新建」。位置规则：当前笔记所在目录，没有选中就是根目录。
                不做目录选择器——一个每次都要回答"建在哪"的弹窗，比多点一次右键更烦。
              */}
              <div className="sidebar__head">
                <button
                  type="button"
                  className="btn btn--small"
                  onClick={() =>
                    setPrompt({
                      kind: 'create',
                      dir: activeNote === null ? '' : parentDir(activeNote.path),
                    })
                  }
                  title={
                    activeNote === null
                      ? '在 Vault 根目录新建'
                      : `在 ${parentDir(activeNote.path) || '根目录'} 新建`
                  }
                >
                  ＋ 新建
                </button>
              </div>
              <div className="sidebar__root" title={vault.root}>
                {vault.root}
              </div>
              <div className="tree">
                {vault.tree.children?.length ? (
                  vault.tree.children.map((child) => (
                    <TreeNode
                      key={child.path}
                      node={child}
                      depth={0}
                      activePath={activeNote?.path ?? null}
                      revealPath={revealPath}
                      onSelect={selectNote}
                      onContextMenu={openMenu}
                    />
                  ))
                ) : (
                  <p className="sidebar__empty">
                    这个文件夹里没有 .md 文件。点上面的「新建」写第一篇。
                  </p>
                )}
              </div>
            </>
          ) : (
            <div className="sidebar__placeholder">
              <p>还没有打开知识库</p>
              <p className="hint">点右上角「选择文件夹」，选一个装着 .md 的目录。</p>
            </div>
          )}
        </aside>

        <section className="content">
          {vault && activeNote ? (
            <>
              <section className="viewer">
                <header className="viewer__header">
                  <nav className="breadcrumb" aria-label="路径">
                    {activeNote.path.split('/').map((segment, index) => (
                      <span key={`${segment}-${index}`} className="breadcrumb__item">
                        {index > 0 && <span className="breadcrumb__sep">/</span>}
                        {segment}
                      </span>
                    ))}
                  </nav>
                  <span className="viewer__meta">{saveLabel}</span>
                </header>

                <NoteEditor
                  ref={editorRef}
                  root={vault.root}
                  note={activeNote}
                  docId={docId}
                  onStateChange={setSaveState}
                  onError={setError}
                  onSaved={() => showFlash('已保存')}
                  onOutlineChange={setOutline}
                  outlineRef={outlineRef}
                />
              </section>
              <Outline ref={outlineRef} items={outline} onJump={jumpTo} />
            </>
          ) : (
            /*
             * 三种"没有正文"的状态必须分开说。合并成一句话的话，删掉当前笔记
             * 之后用户会看到「写第一篇」——而库里明明还有 999 篇，他会以为
             * 笔记全没了。这是本刀唯一允许改文案的地方。
             */
            <div className="welcome">
              <div className="welcome__mark">◈</div>
              <h1>把知识，安放在自己的岛上。</h1>
              {vault === null ? (
                <>
                  <p>还没有打开知识库。</p>
                  <p className="hint">
                    点右上角「选择文件夹」，选一个装着 .md 的目录。{runtime}
                  </p>
                </>
              ) : vault.totalFiles === 0 ? (
                <>
                  <p>这个库里还没有笔记。</p>
                  <p className="hint">点左上角「＋ 新建」写第一篇。{runtime}</p>
                </>
              ) : (
                <>
                  <p>还没选中笔记。</p>
                  <p className="hint">
                    从左侧选一篇开始写，或按 Cmd/Ctrl+P 快速打开。{runtime}
                  </p>
                </>
              )}
            </div>
          )}
        </section>
      </main>

      {/*
        条件渲染而不是 `open` prop：关闭即卸载，查询与选中项自然归零。
        放在 `.app` 的最后、且用 fixed 定位——它不参与 workspace 的
        布局计算，打开浮层不会让正文重新排版（那会连带触发 CM6 重测量）。

        # 为什么这里可以在渲染期读 ref

        `react/refs` 这条规则防的是「ref 变了但组件没重渲染，读到旧值」。
        这里不成立：`quickItems.current` 只在一处被写（`applyVault`），
        而它在**同一个函数里**紧挨着 `setVault`。也就是说 ref 的换代必然
        伴随一次 state 更新，那次更新必然带着 `paletteOpen` 一起提交。
        反过来，浮层只在 `paletteOpen` 为真时渲染，而 `paletteOpen` 只能
        由「用户按键」或「选中一项」改变——两者都发生在扫描之后。

        换成 state 也能过这条规则，但那样每次无关重渲染（保存状态每
        300–500ms 一次）都要多比一次 1000 元素的数组引用；换成
        `useMemo` 则等于把「扫描完成时算一次」降级成「缓存没命中就算」。
        ref 是这里唯一能给出硬保证的写法。
      */}
      {paletteOpen && (
        // oxlint-disable-next-line react/refs -- 见上方「为什么这里可以在渲染期读 ref」
        <QuickOpen items={quickItems.current} onOpen={openFromPalette} onClose={closePalette} />
      )}

      {/*
        搜索浮层与快速打开互斥（两个快捷键各自挡住对方），所以这里不需要
        考虑"谁盖在谁上面"。`vault` 判空只为让 `root` 的类型收窄——没有库时
        快捷键根本不会开它。
      */}
      {searchOpen && vault && (
        <SearchOverlay
          root={vault.root}
          activePath={activeNote?.path ?? null}
          onPrepare={prepareSearch}
          onOpen={openFromSearch}
          onClose={closeSearch}
        />
      )}

      {/*
        右键菜单与名字浮层都渲染在最外层：它们是 fixed 定位，不参与 workspace
        布局，打开时不会让正文重新排版（那会连带触发 CM6 重测量）。
      */}
      {menu !== null && (
        <ContextMenu x={menu.x} y={menu.y} items={menuItemsFor(menu.node)} onClose={closeMenu} />
      )}

      {prompt?.kind === 'create' && (
        <NamePrompt
          title="新建笔记"
          initial=""
          confirmLabel="创建"
          onSubmit={(name) => submitCreate(prompt.dir, name)}
          onClose={closePrompt}
        />
      )}

      {prompt?.kind === 'rename' && (
        <NamePrompt
          title="重命名"
          initial={prompt.name}
          confirmLabel="重命名"
          onSubmit={(name) => submitRename(prompt.path, name)}
          onClose={closePrompt}
        />
      )}

      {/*
        删除的二次确认。文案必须把三件事说全：移到哪里、**不是**永久删除、
        但应用里没有恢复入口。少说最后一句，"移到回收站"会被理解成
        "随时能撤销"——那是我们给不了的承诺。
      */}
      {deleteTarget !== null && (
        <ConfirmDialog
          title="删除这篇笔记？"
          confirmLabel="删除"
          onSubmit={() => submitDelete(deleteTarget.path)}
          onClose={closeDelete}
        >
          <p>
            <span className="confirm__name">{deleteTarget.name}</span> 会被移到{' '}
            <code className="confirm__path">.knowisle/trash/</code>。
          </p>
          <p className="confirm__note">
            不是永久删除，文件还在磁盘上；但应用里<strong>没有恢复入口</strong>，要找回得自己去文件夹翻。
          </p>        </ConfirmDialog>
      )}
    </div>
  )
}

/** 把保存状态翻成一句人话。空串表示还没开始编辑，不显示。 */
function describeSaveState(state: SaveState | null): string {
  if (state === null) return ''
  if (state.lastError !== null) {
    return state.lastError.code === 'CONFLICT' ? '冲突 · 未保存' : '保存失败'
  }
  if (state.dirty) return '保存中…'
  if (state.lastSavedMtime !== null) {
    return `已保存 ${new Date(state.lastSavedMtime).toLocaleTimeString('zh-CN')}`
  }
  return ''
}
