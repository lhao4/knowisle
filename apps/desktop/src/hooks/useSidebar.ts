import { useCallback, useEffect, useState, type RefObject } from 'react'

import type { FileNode } from '../api'
import type { ContextMenuItem } from '../components/ContextMenu'
import type { NoteEditorHandle } from '../components/NoteEditor'
import { parentDir } from './path'

/**
 * 侧栏开合。`'1'` 展开 / `'0'` 折叠，缺省是展开。
 *
 * # 为什么不需要 `index.html` 的内联脚本
 *
 * 主题必须在样式生效前落到 `<html>` 上，所以它有一份"pre-React 的"内联副本。
 * 侧栏不一样：它是 React 画出来的，`.app` 与 `.sidebar` 在**同一次提交**里
 * 出现。在 `useState` 的初始化器里同步读，读到的就是首帧的值——不存在
 * 「先按 300px 画一帧、再变 0」的中间态。
 */
const SIDEBAR_KEY = 'knowisle.sidebar'

/** 读侧栏开合。隐私模式下 `localStorage` 的 getter 会抛，回落到展开。 */
function readSidebarOpen(): boolean {
  try {
    return localStorage.getItem(SIDEBAR_KEY) !== '0'
  } catch {
    return true
  }
}

/**
 * 正在输入名字的浮层。
 *
 * 两种意图共用一个组件，但**位置来源不同**：
 * - `create` 只需要目录（当前笔记所在目录，没有选中就是根目录）；
 * - `rename` 需要旧路径（新名字只在同一个目录内生效）。
 */
export type PromptState =
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

interface UseSidebarOptions {
  /**
   * 当前打开的笔记。只用来算 `createDir`（「新建」建到哪）——
   * 侧栏不拥有它，`openNote` 那一条链路留在 `App`。
   */
  activeNote: { path: string } | null
  /** 有没有打开库。`Mod+N` 在没有库时空操作。 */
  hasVault: boolean
  /**
   * 编辑器句柄。折叠侧栏前要把焦点从树里救出来，用的就是它。
   *
   * 与下面的 `focusEditor` 是**两件事**：这里要的是"立刻把焦点放到编辑器"，
   * 而 `focusEditor` 带按帧重试（新建之后编辑器可能还没挂载）。折叠侧栏时
   * 编辑器一定已经在，重试没有意义。
   */
  editorRef: RefObject<NoteEditorHandle | null>
  /** 带重试的焦点归还。关名字浮层 / 关删除确认之后用。 */
  focusEditor: () => void
  /**
   * 另外两个域的全屏浮层。`Mod+N` 必须知道它们：`Mod+N` 再开一层，
   * 屏幕上就有两个 `.prompt`，而 Esc 只关最上面那层——用户会以为卡住了。
   */
  paletteOpen: boolean
  searchOpen: boolean
  trashOpen: boolean
}

/**
 * 侧栏域：折叠态、右键菜单、新建 / 重命名的名字浮层、删除的二次确认。
 *
 * # 为什么只有这四个 state
 *
 * 判据是「谁读它、谁写它」。`vault` / `activeNote` / `revealPath` 都**写在别处**：
 * `revealPath` 的写入方横跨新建、改名、删除、回收站恢复、换库五个动作，
 * 塞进这里就要给另外两个域各开一个 setter 口子，那是把耦合换个地方放。
 * 它们留在 `App.tsx`，本 hook 只把"要露哪一条"当成入参/出参。
 *
 * # 不拆组件树
 *
 * 抽 hook 只移动状态与副作用，`App.tsx` 的渲染结构、类名、文案、aria-* 一个
 * 字节不动。这是它比"拆组件"优先的原因：GUI 验收的判据（DOM 选择器、文案）
 * 一条都不用改。
 */
export function useSidebar({
  activeNote,
  hasVault,
  editorRef,
  focusEditor,
  paletteOpen,
  searchOpen,
  trashOpen,
}: UseSidebarOptions) {
  /**
   * 侧栏是否展开。初值由初始化器同步读盘，理由见 `SIDEBAR_KEY`。
   *
   * **折叠是 CSS 隐藏，不是条件渲染**（`.app--sidebar-collapsed`）。
   * `{sidebarOpen && <aside>}` 会在折叠一次之后把整棵子树卸载，而
   * `TreeNode` 的展开状态是组件内 `useState`——用户手动展开过的目录会
   * 全部回到初值，再展开时看起来像"我的树被重置了"。
   */
  const [sidebarOpen, setSidebarOpen] = useState(readSidebarOpen)
  /** 正在输入名字的浮层。`null` 表示没有。 */
  const [prompt, setPrompt] = useState<PromptState | null>(null)
  /** 右键菜单。 */
  const [menu, setMenu] = useState<MenuState | null>(null)
  /** 等待二次确认的删除目标。`null` 表示没有确认框。 */
  const [deleteTarget, setDeleteTarget] = useState<DeleteTarget | null>(null)

  // 写回。`useEffect` 就够了：首帧敏感的是"读"，那件事在初始化器里做完了。
  useEffect(() => {
    try {
      localStorage.setItem(SIDEBAR_KEY, sidebarOpen ? '1' : '0')
    } catch {
      // 隐私模式 / 配额满：本次会话内照样生效，只是下次启动回到展开。
    }
  }, [sidebarOpen])

  /**
   * 折叠 / 展开侧栏。快捷键与顶栏按钮共用这一条路径。
   *
   * # 折叠前先把焦点救出来
   *
   * 焦点可能正在树里（用户点了一行再按 `Mod+\`）。`display:none` 会把聚焦
   * 元素从文档里摘掉，焦点掉到 `<body>` 上——之后所有按键都没有落点，
   * 用户会以为应用卡死了。只在焦点确实位于侧栏内时才动它：焦点本来就在
   * 正文里的话，任何抢焦点的动作都是多余的。
   */
  const toggle = useCallback(() => {
    if (sidebarOpen) {
      const active = document.activeElement
      if (active instanceof Element && active.closest('.sidebar') !== null) {
        editorRef.current?.focus()
      }
    }
    setSidebarOpen(!sidebarOpen)
  }, [sidebarOpen, editorRef])

  /** 关闭名字浮层并把焦点还给编辑器。取消与成功共用这一条路径。 */
  const closePrompt = useCallback(() => {
    setPrompt(null)
    focusEditor()
  }, [focusEditor])

  const openMenu = useCallback((node: FileNode, x: number, y: number) => {
    setMenu({ node, x, y })
  }, [])

  const closeMenu = useCallback(() => setMenu(null), [])

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
   * 「新建」的目标目录：当前笔记所在目录，没有选中就是根目录。
   *
   * 侧栏的「＋ 新建」、欢迎页的「写第一篇」与 `Mod+N` 共用它。三处各算一遍的话，
   * 某天改了一处就会出现"按钮建在日记里、快捷键建在根目录"这种谁也说不清的行为。
   */
  const createDir = activeNote === null ? '' : parentDir(activeNote.path)

  /** 打开「新建」浮层。三处入口共用，所以它必须自己算 `createDir`。 */
  const openCreate = useCallback(() => {
    setPrompt({ kind: 'create', dir: createDir })
  }, [createDir])

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
   * 换库时清掉上一层浮层。
   *
   * 只清 `prompt` 与 `menu`：它们挂着**上一个库的路径**，换库之后点下去会
   * 对着新库做旧库的动作。`deleteTarget` 不在这里——换库时它不可能开着
   * （它挂在树节点的右键菜单后面，而菜单刚刚被清掉）。
   */
  const reset = useCallback(() => {
    setPrompt(null)
    setMenu(null)
  }, [])

  /** 屏幕上是否有任意一层浮层 / 菜单。`Mod+N` 据此判断要不要装死。 */
  const overlayOpen =
    prompt !== null ||
    paletteOpen ||
    searchOpen ||
    menu !== null ||
    deleteTarget !== null ||
    trashOpen

  /**
   * `Cmd/Ctrl+N` 新建笔记。
   *
   * # 为什么是 window 级
   *
   * 「新建」与焦点无关：用户点着树、看着大纲、刚关掉一个浮层，都该能按。
   * 加粗 / 斜体正相反——它们必须有文档选区，所以挂在 CM6 的 keymap 上
   * （见 `NoteEditor`）。两种注册方式不能混：把 `Mod+B` 挂到 window 上，
   * 名字输入框里按一下就会改到背后的文档。
   *
   * # 任一闪层打开时空操作
   *
   * 不是为了"优雅"：`Mod+N` 再开一层，屏幕上就有两个 `.prompt`，而 Esc
   * 只关最上面那层——用户会以为卡住了。快捷键不叠层是硬约束。
   *
   * # 无库时也要 `preventDefault`
   *
   * WebView2 里 `Ctrl+N` 是"新建窗口"。不拦的话，用户在欢迎页按一下就会
   * 多出一个空窗口，而这个应用没有任何多窗口设计。
   */
  useEffect(() => {
    const onKeyDown = (e: globalThis.KeyboardEvent) => {
      if (e.repeat || e.altKey || e.shiftKey) return
      if (!e.metaKey && !e.ctrlKey) return
      if (e.key !== 'n' && e.key !== 'N') return
      e.preventDefault()
      if (!hasVault || overlayOpen) return
      setPrompt({ kind: 'create', dir: createDir })
    }

    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [hasVault, overlayOpen, createDir])

  /**
   * `Cmd/Ctrl+\` 折叠 / 展开侧栏（Notion 的键位）。
   *
   * # 为什么不判 `hasVault`
   *
   * 侧栏在没打开库时是一个 placeholder，收起来同样能让欢迎页占满整宽。
   * 更重要的是：顶栏那个按钮**任何时候都在**，快捷键却只在有库时生效的话，
   * 同一次开合会因为"有没有库"而有两种答案。同一条路径，同一个判据。
   *
   * # 为什么不判浮层
   *
   * 浮层是 fixed 定位、盖在最上面，侧栏在后面收起来既看不见也不打断输入。
   * 唯一有代价的是 `Mod+N`——那会叠层，所以它单独判（见上一条）。
   *
   * # `e.key` 之外还要看 `e.code`
   *
   * 部分布局（以及某些输入法开启时的 WebView）给的 `key` 不是反斜杠。
   * 物理键位是同一个，所以 `Backslash` 也认。带 Shift 的组合已经被上面的
   * 早退挡住了，不会误伤 `|`。
   */
  useEffect(() => {
    const onKeyDown = (e: globalThis.KeyboardEvent) => {
      if (e.repeat || e.altKey || e.shiftKey) return
      if (!e.metaKey && !e.ctrlKey) return
      if (e.key !== '\\' && e.code !== 'Backslash') return
      e.preventDefault()
      toggle()
    }

    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [toggle])

  return {
    open: sidebarOpen,
    toggle,
    prompt,
    menu,
    deleteTarget,
    createDir,
    openCreate,
    openMenu,
    closeMenu,
    closePrompt,
    closeDelete,
    menuItemsFor,
    reset,
  }
}
