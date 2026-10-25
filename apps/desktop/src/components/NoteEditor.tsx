import { markdown } from '@codemirror/lang-markdown'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { HighlightStyle, syntaxHighlighting, syntaxTree } from '@codemirror/language'
import { EditorState, type Range } from '@codemirror/state'
import {
  Decoration,
  EditorView,
  ViewPlugin,
  drawSelection,
  highlightActiveLine,
  keymap,
  lineNumbers,
  type DecorationSet,
  type ViewUpdate,
} from '@codemirror/view'
import { tags } from '@lezer/highlight'
import { useEffect, useImperativeHandle, useRef, type Ref, type RefObject } from 'react'

import {
  describeError,
  noteEdit,
  noteFlush,
  noteStatus,
  type NoteContent,
  type SaveState,
  type SearchHit,
} from '../api'
import { toggleMark, type MarkKind } from '../lib/marks'
import { activeIndexAt, collectOutline, sameHeadingSequence, type OutlineItem } from '../lib/outline'
import type { OutlineHandle } from './Outline'

/**
 * 把一次按键合并成一次 IPC 的间隔。
 *
 * 与 Rust 侧的 1.5 秒节流是**两层，各管各的**：
 *
 * - 这里管 **IPC 频率**：10 字/秒的输入不该产生 10 次跨进程调用。
 * - Rust 侧管 **落盘时机**：1.5 秒是给用户的承诺，与前端快慢无关。
 *
 * 两个数字没有耦合关系，改一个不需要动另一个。
 */
const IPC_COALESCE_MS = 300

/**
 * Markdown 语法高亮。
 *
 * 必须用 `HighlightStyle` 而不是在 CSS 里写 `.tok-heading`——
 * CM6 生成的是混淆过的类名（`ͼ1a` 之类），文档里的 `tok-*` 只是旧版本的痕迹。
 * 这里让颜色指向 CSS 变量，主题切换仍然只改一层 token。
 *
 * # 这里**不**写字号
 *
 * 字号交给行装饰（见下方 `headingLineClass`），原因在 `HeaderMark`：
 * `@lezer/markdown` 把 `#` 单独映射成 `tags.processingInstruction`，
 * 它比 `ATXHeading1/...` 更具体，所以标题正文放大到 1.6em 时 `#` 仍是 13px——
 * 视觉上像渲染错误。行级字号才能让整行（含标记）等比缩放。
 */
const kiHighlightStyle = HighlightStyle.define([
  { tag: tags.heading, fontWeight: '600', color: 'var(--ki-text-primary)' },
  // 六级最轻，靠"更小 + 更淡"与前五级分开。字号不在这里（见上方说明），
  // 但 `HighlightStyle` 命中最具体的那条时会**整条替换**父标签规则，
  // 所以字重必须跟着写一遍，否则 H6 会掉回 400。
  { tag: tags.heading6, fontWeight: '600', color: 'var(--ki-text-muted)' },
  { tag: tags.strong, fontWeight: '600' },
  { tag: tags.emphasis, fontStyle: 'italic' },
  { tag: [tags.link, tags.url], color: 'var(--ki-accent)' },
  // 只上色。底色交给 `InlineCode` 的 mark 装饰：`CodeText`（围栏代码块的内容）
  // 也映射到同一个 `monospace` tag，在这里加背景会让代码块每行出现一块
  // 只覆盖文字宽度的灰底，看起来像坏掉。
  { tag: tags.monospace, color: 'var(--ki-text-primary)' },
  { tag: [tags.processingInstruction, tags.labelName], color: 'var(--ki-text-muted)' },
  { tag: tags.quote, color: 'var(--ki-text-muted)' },
  { tag: tags.list, color: 'var(--ki-accent)' },
])

/** 行内代码的底色（只覆盖 `` `code` `` 本身，不碰围栏代码块）。 */
const inlineCodeMark = Decoration.mark({ class: 'cm-ki-inline-code' })

/**
 * 行装饰：标题字号 + 引用块左边线。
 *
 * 两者都必须落在**行**上：
 *
 * - 引用块要跟着整块走（含块内空行），给行内 span 加 border 会在换行处断成锯齿。
 * - 标题要让 `#` 与正文一起缩放，理由见 `kiHighlightStyle` 的注释。
 *
 * 类名是我们自己起的，CM6 不会混淆它（被混淆的是它内部生成的语法 token 类名）。
 * 字号/颜色都指向 `--ki-*` token，主题切换只改一层。
 */
function headingLineClass(level: string): string {
  return `cm-ki-h${level}`
}

function buildDecorations(view: EditorView): DecorationSet {
  const { doc } = view.state
  /** 行首 → 该行要挂的类名。一行可能同时是引用块与标题（`> # 标题`）。 */
  const lineClasses = new Map<number, string[]>()
  const marks: Range<Decoration>[] = []

  const addLine = (pos: number, cls: string) => {
    const from = doc.lineAt(pos).from
    const classes = lineClasses.get(from)
    if (classes === undefined) lineClasses.set(from, [cls])
    else if (!classes.includes(cls)) classes.push(cls)
  }

  for (const { from, to } of view.visibleRanges) {
    syntaxTree(view.state).iterate({
      from,
      to,
      enter: (node) => {
        const heading = /^(?:ATX|Setext)Heading([1-6])$/.exec(node.name)
        if (heading !== null) {
          addLine(node.from, headingLineClass(heading[1]))
          return
        }
        if (node.name === 'InlineCode') {
          marks.push(inlineCodeMark.range(node.from, node.to))
          return
        }
        if (node.name === 'Blockquote') {
          // 嵌套引用会命中同一个 Blockquote 多次（`> > a` 是两层），按行首去重。
          const first = doc.lineAt(node.from).number
          const last = doc.lineAt(Math.max(node.from, node.to - 1)).number
          for (let n = first; n <= last; n += 1) addLine(doc.line(n).from, 'cm-ki-quote')
        }
      },
    })
  }

  const ranges: Range<Decoration>[] = marks
  for (const [from, classes] of lineClasses) {
    ranges.push(Decoration.line({ class: classes.join(' ') }).range(from))
  }
  // `sort` 交给 Decoration.set：可见区间与嵌套层级都不保证按位置递增。
  return Decoration.set(ranges, true)
}

const markdownDecorations = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet

    constructor(view: EditorView) {
      this.decorations = buildDecorations(view)
    }

    update(update: ViewUpdate) {
      // 第三个条件不能省：Lezer 是增量异步解析的，编辑后树可能在下一次
      // update（不是 viewport 变化）才补齐，少了它会漏画新出现的引用块。
      if (
        update.docChanged ||
        update.viewportChanged ||
        syntaxTree(update.state) !== syntaxTree(update.startState)
      ) {
        this.decorations = buildDecorations(update.view)
      }
    }
  },
  { decorations: (v) => v.decorations },
)

export interface NoteEditorHandle {
  /** 编辑器里此刻的完整正文。切换笔记前用它把最后几个字捞出来。 */
  getContent: () => string
  /** 把光标送到 `pos` 并把那一行滚到视口顶部。大纲点击走这里。 */
  jumpTo: (pos: number) => void
  /**
   * 把焦点交还给编辑器。
   *
   * # 为什么必须是 `view.focus()`，而不是 `dispatch` 一个 selection
   *
   * 浮层关闭时，编辑器里的光标**本来就没动过**——`EditorState` 里的
   * selection 一直在那儿，只是 DOM 焦点被 input 拿走了。所以这里要做的
   * 只是把 DOM 焦点还回去，CM6 会自己按 state 恢复绘制。
   *
   * 反过来，`dispatch({ selection })` 会**新建**一个 selection：只要锚点
   * 算错一个字符（比如把 `anchor` 写成 `head: 0`），用户关掉浮层就会发现
   * 光标跑到了文档开头。用一个不需要知道位置的动作，就没有算错的机会。
   */
  focus: () => void
  /**
   * 跳到一条搜索命中：选中命中文本（原生选中就是高亮），并把那一行滚到视口中间。
   *
   * # 为什么这里要处理"还没挂载"的情况
   *
   * 跨文件跳转的顺序是「`openNote(path)` → resolve → 调本方法」。但
   * `openNote` 的 `setActiveNote` 只是**排了一次渲染**，resolve 的时候
   * 新文档往往还没挂载。此刻 `viewRef` 指向的是即将被销毁的旧视图——
   * 在它上面 dispatch，用户看到的会是「跳过去了，但光标在开头」。
   *
   * 所以路径对不上时先存起来，由挂载 effect 在创建新视图之后立刻消费。
   * 同文件跳转（路径相同）则直接应用，不绕这一圈。
   */
  revealHit: (hit: SearchHit) => void
  /**
   * 上层已经把这份内容完整冲刷过（`noteEdit` → `noteFlush`）：丢掉本地缓冲。
   *
   * # 为什么需要它
   *
   * `getContent()` 只是"读"，不改变 300ms 合并窗口的状态。上层冲刷完之后，
   * 那个定时器仍然会到点触发，把**同一份内容**再发一次。平时这只是一次
   * 无谓的 IPC；改名之后它是错的——闭包里的路径是旧的，这一次重发会在
   * 新文件旁边重新造出一个旧文件。
   *
   * # 为什么带内容做参数
   *
   * 只有"缓冲里就是刚冲刷的那一份"才允许丢。IPC 往返期间用户又打了字时，
   * `pendingRef` 已经是新字符串，那部分字必须留着——按内容比对是唯一
   * 不需要知道"现在过了多久"的判据。
   */
  commitFlushed: (content: string) => void
  /**
   * **无条件**丢掉本地缓冲，不发任何 IPC。删除笔记之后调。
   *
   * # 为什么不能复用 `commitFlushed`
   *
   * 那个函数按内容比对、只在"缓冲正是刚冲刷的那一份"时才丢——因为改名之后
   * 多出来的一次重发是错的。删除的处境不同：这篇文档马上就要从磁盘上消失，
   * 缓冲里剩下的任何内容都不该再发出去（发了就是"删除之后文件又回来了"）。
   *
   * # 为什么只清前端不够
   *
   * 这只挡得住 300ms 合并窗口里的那一次 `noteEdit`。挡不住的是**已经在飞的
   * IPC**：`flushActiveNote` 的 `noteFlush` 可能正走在路上。所以 Rust 侧
   * 还有一道 `autosave::clear_for_delete`（把槽位键置 None），两层各管一段。
   *
   * # 为什么必须在 `setActiveNote(null)` 之前调
   *
   * 编辑器一卸载，cleanup 会再发一次 flush——那次会落在已删除的路径上。
   * 清空缓冲之后，cleanup 里的 `content !== null` 判断为假，它自然什么都不做。
   */
  discardPending: () => void
}

/**
 * 把一条命中应用到一个**已经存在**的 `EditorView` 上。
 *
 * 偏移量全部来自 Rust，单位是 UTF-16 码元——与 CM6 的文档位置、以及
 * JavaScript 的字符串下标完全一致，不需要任何换算。
 *
 * 仍然要夹紧：索引是几秒前建的，磁盘上的文件可能已被别的程序改短，
 * 而 `dispatch` 遇到越界位置会直接抛。
 */
function applyHit(view: EditorView, hit: SearchHit): void {
  const doc = view.state.doc
  const number = Math.min(Math.max(hit.line, 1), doc.lines)
  const line = doc.line(number)
  const from = line.from + Math.min(hit.column, line.length)
  const to = line.from + Math.min(Math.max(hit.columnEnd, hit.column), line.length)
  view.dispatch({
    selection: { anchor: from, head: to },
    // `center` 而不是 `start`：命中行的上下文比"顶格"重要，
    // 上下各留一点，用户能看清这句话属于哪一段。
    effects: EditorView.scrollIntoView(from, { y: 'center' }),
  })
  view.focus()
}

/**
 * 加粗 / 斜体：把纯函数的结果落到视图上。
 *
 * # 为什么挂在 CM6 的 keymap 上，而不是像 `Mod+P` 那样挂 window
 *
 * 这两个键**必须有文档选区**才有意义。挂 window 的话，快速打开浮层、名字
 * 输入框、搜索框里按 `Mod+B` 也会改文档——用户看不见背后的编辑器，却已经
 * 把内容改了。CM6 的 keymap 只在编辑器持有焦点时才跑，这条边界是白送的。
 *
 * # 一次 dispatch
 *
 * `changes` 与 `selection` 必须同属一个 spec，理由见 `MarkEdit` 的注释。
 * 返回 `true` 表示这个键被消费掉了；`false`（选区里全是空行）时让 CM6
 * 继续往下找——绑定的 `preventDefault` 会挡住 WebView 的默认行为。
 */
function applyMark(view: EditorView, mark: MarkKind): boolean {
  const { from, to } = view.state.selection.main
  const edit = toggleMark(view.state.doc.toString(), from, to, mark)
  if (edit === null) return false
  view.dispatch({ changes: edit.changes, selection: edit.selection })
  return true
}

interface NoteEditorProps {
  root: string
  note: NoteContent
  /**
   * **文档身份**。打开一篇新笔记时自增，重命名时不变。
   *
   * 编辑器重建只认它，不认 `note.path`：改名会让 path 变，而撤销栈、光标、
   * 滚动位置都属于"同一篇文档"，不该为了一次改名全部丢掉。
   * `note.path` 退化成纯元数据——IPC 参数、树高亮、breadcrumb 用它。
   */
  docId: number
  ref?: Ref<NoteEditorHandle>
  onStateChange: (state: SaveState) => void
  onError: (message: string | null) => void
  /** 手动保存成功（`Mod-s`）。与轮询出来的「已保存」是两条路径，互不依赖。 */
  onSaved: () => void
  /**
   * 低频回调：**只有标题集合变化**时才调。
   *
   * 必须是稳定引用（`setState` 或 `useCallback`）——它在挂载时被捕获，
   * 换身份不会重建编辑器，但也不会生效。
   */
  onOutlineChange: (items: OutlineItem[]) => void
  /**
   * 高频通道：光标所在章节的变化直接打到面板的 DOM 上。
   *
   * 这里刻意**不用回调**：回调必然要配一个 `setState` 才看得见效果，
   * 而连打 100 个字符就是 100 次渲染。句柄让面板自己做 `classList.toggle`。
   */
  outlineRef: RefObject<OutlineHandle | null>
}

/**
 * CodeMirror 6 编辑器。
 *
 * # 四条设计约束
 *
 * 1. **文档内容不进 React state。** 每次按键都 `setState` 会让 React 重渲染整棵树，
 *    而 CM6 自己维护 `EditorState`。这里只在 ref 里持有 `EditorView`，
 *    React 完全不参与输入路径。
 * 2. **只有 `docId` 变化才重建编辑器。** 用 `note.content` 做依赖会导致
 *    "外部改了文件 → 内容变了 → 编辑器重建 → 光标丢失"；用 `note.path` 做依赖
 *    会导致"改个文件名 → 撤销栈与光标全丢"。文档身份与它在磁盘上的名字是两件事，
 *    前者由上层自增，后者只是元数据。
 * 3. **切换笔记时的顺序由上层控制。** 合并窗口（300ms）里可能还有没发出去的字符，
 *    如果上层先 flush 再卸载，那几个字就丢了。所以这里暴露 `getContent()`，
 *    由上层「先取内容 → 发 noteEdit → flush → 再切换」。
 * 4. **大纲的标题集合与高亮走两条通道。** 前者进 React state（低频），
 *    后者直接操作 DOM（高频）。合并的那一天，渲染次数就与按键次数同阶了。
 */
export function NoteEditor({
  root,
  note,
  docId,
  ref,
  onStateChange,
  onError,
  onSaved,
  onOutlineChange,
  outlineRef,
}: NoteEditorProps) {
  const hostRef = useRef<HTMLDivElement>(null)
  const viewRef = useRef<EditorView | null>(null)
  /** 当前视图对应的笔记路径。跨文件跳转、IPC 目标路径都用它。 */
  const pathRef = useRef<string | null>(null)
  /** 还没落地的搜索命中（视图尚未切换到位）。 */
  const pendingHitRef = useRef<SearchHit | null>(null)
  /** 最近一次尚未发出的正文。`null` 表示没有待发内容。 */
  const pendingRef = useRef<string | null>(null)
  /** 在途的 IPC 定时器。`null` 表示当前没有等待中的合并窗口。 */
  const timerRef = useRef<number | null>(null)

  useImperativeHandle(
    ref,
    () => ({
      getContent: () => viewRef.current?.state.doc.toString() ?? pendingRef.current ?? '',
      jumpTo: (pos: number) => {
        const view = viewRef.current
        if (view === null) return
        // 夹紧到文档范围内：大纲是上一帧算出来的，文档刚变短时 pos 可能越界，
        // 而 `dispatch` 遇到越界的位置会直接抛。
        const target = Math.max(0, Math.min(pos, view.state.doc.length))
        view.dispatch({
          selection: { anchor: target },
          // `y: 'start'` 才是验收要的「该行滚到顶部」。默认的 `nearest`
          // 只会保证可见，视觉上像是没跳。
          effects: EditorView.scrollIntoView(target, { y: 'start' }),
        })
        view.focus()
      },
      focus: () => {
        // `focus()` 在视图已销毁时是空操作（`viewRef` 被清成 null），
        // 所以这里不需要额外的存活判断。
        viewRef.current?.focus()
      },
      revealHit: (hit) => {
        const view = viewRef.current
        if (view === null || pathRef.current !== hit.path) {
          // 跨文件跳转还在 IPC 往返里：新视图没挂载之前，旧视图不能碰。
          pendingHitRef.current = hit
          return
        }
        applyHit(view, hit)
      },
      commitFlushed: (content) => {
        // 见 `NoteEditorHandle.commitFlushed`：按内容比对，不按时间猜。
        if (pendingRef.current !== content) return
        pendingRef.current = null
        if (timerRef.current !== null) {
          window.clearTimeout(timerRef.current)
          timerRef.current = null
        }
      },
      discardPending: () => {
        // 见 `NoteEditorHandle.discardPending`：不比对、不发 IPC、不问理由。
        pendingRef.current = null
        if (timerRef.current !== null) {
          window.clearTimeout(timerRef.current)
          timerRef.current = null
        }
      },
    }),
    [],
  )

  /**
   * 让 IPC 目标路径跟随 props。
   *
   * 改名**不重建**编辑器（挂载依赖是 `docId`），所以闭包里那个
   * `pathAtMount` 会一直指着旧名字。所有 `noteEdit` 调用都读这个 ref，
   * 改名之后它们自然落到新路径上。
   *
   * 写在 effect 里而不是渲染期直接赋值：渲染期写 ref 在并发渲染下会把
   * 一棵被丢弃的树的路径写进去，而这里没有任何东西能发现它写错了。
   *
   * 顺序上它是安全的：换文档时 React 先跑完所有 cleanup 再跑 effect，
   * 所以挂载 effect 的 cleanup 读到的仍然是**旧**路径（那才是它该写的）。
   */
  useEffect(() => {
    pathRef.current = note.path
  }, [note.path])

  useEffect(() => {
    const host = hostRef.current
    if (host === null) return

    // 全部复制成局部常量：cleanup 跑的时候 props 与 ref 都可能已经变了，
    // 而这次清理要处理的明明是"挂载时的那一篇"。
    // 路径变化会重建整个编辑器（见下方依赖数组），所以挂载时的值就是这一篇的值。
    const rootAtMount = root
    const pathAtMount = note.path
    pathRef.current = pathAtMount
    // 基线只在换文档时被 Rust 采信一次，所以始终传打开时的 mtime。
    const baseMtimeAtMount = note.mtime
    const initialDoc = note.content

    const clearTimer = () => {
      if (timerRef.current === null) return
      window.clearTimeout(timerRef.current)
      timerRef.current = null
    }

    /**
     * IPC 目标路径。**每次调用时现取**，不能在挂载时算一次：
     * 改名会换路径而不换视图，闭包里的 `pathAtMount` 从此就是旧名字。
     * `pathRef` 为空只可能发生在 cleanup 之后，那时也没人再调它了。
     */
    const ipcPath = () => pathRef.current ?? pathAtMount

    const flushPending = () => {
      const content = pendingRef.current
      if (content === null) return
      pendingRef.current = null
      noteEdit(rootAtMount, ipcPath(), content, baseMtimeAtMount)
        .then(onStateChange)
        .catch((err: unknown) => onError(describeError(err)))
    }

    /**
     * 立刻落盘（`Mod-s`）。
     *
     * # 为什么不能直接 `noteFlush`
     *
     * `noteFlush` 只冲刷 **Rust 槽位**里的内容。按键后 300ms 内，
     * 最新的字符还躺在 `pendingRef` 里没发出去——此时 flush 会拿到
     * 「没有待写内容」并立刻返回，用户看到的「已保存」是上一版。
     * 所以顺序必须是「先取缓冲 → 发 noteEdit → 再 flush」，
     * 与切换笔记时是同一条规矩。
     *
     * # 连按 10 次
     *
     * 第一次把内容写下去之后，`pendingRef` 与槽位都是空的，
     * 后续每次 `noteFlush` 直接返回上一次的回执（`Ok(None)` / `Ok(Some)`），
     * **不会重复写盘**——mtime 保持不变，这正是验收里那条判据。
     */
    const saveNow = () => {
      const content = pendingRef.current
      pendingRef.current = null
      clearTimer()
      // 整条链只认一个路径：中途改名的话，noteEdit 与 noteFlush 必须落在
      // 同一篇上，否则 flush 冲刷的是另一篇的槽位，会静默地什么都不做。
      const target = ipcPath()

      const pushed: Promise<void> =
        content === null
          ? Promise.resolve()
          : noteEdit(rootAtMount, target, content, baseMtimeAtMount).then(() => undefined)

      pushed
        .then(() => noteFlush(rootAtMount, target, false))
        .then(() => {
          onSaved()
          return noteStatus(rootAtMount, target)
        })
        .then(onStateChange)
        .catch((err: unknown) => {
          onError(describeError(err))
          // 冲突横幅由 `saveState.lastError` 驱动，只报错不刷新状态的话
          // 它要等下一轮轮询（500ms）才出现。
          noteStatus(rootAtMount, target)
            .then(onStateChange)
            .catch(() => {
              // 状态也读不到时，上面的错误信息已经是全部信息了。
            })
        })
    }

    const schedule = () => {
      // 已有在途窗口就不再重置——这是 throttle 而不是 debounce。
      // 重置会让"连续输入时永不发送"，与 Rust 侧的错误做法一模一样。
      if (timerRef.current !== null) return
      timerRef.current = window.setTimeout(() => {
        timerRef.current = null
        flushPending()
      }, IPC_COALESCE_MS)
    }

    /* ───────────────────── 大纲 ───────────────────── */

    /**
     * 上一次发给 React 的大纲。**只在标题集合变化时被替换**，
     * 所以它同时是「低频通道」的判重基准和高频通道的查询表。
     */
    let lastItems: OutlineItem[] = []
    /** 上一次的高亮下标。高频路径上它挡住绝大多数无变化的调用。 */
    let lastActive = -1
    /** 挂载后是否发过一次。空文档也要发一次，否则上层留着上一篇的标题。 */
    let emitted = false

    /**
     * 低频道：**标题序列**变了才调 `onOutlineChange`（一次 React 渲染）。
     *
     * # 为什么 `pos` 要就地改，而不是跟着重渲染
     *
     * `pos` 是绝对偏移，在正文里打一个字，下面所有标题的 `pos` 都 +1。
     * 若把它纳入判重，每个字符都会触发一次大纲重渲染——验收 3 会直接挂。
     * 但它又必须是新鲜的，否则「先打几个字、再点大纲」会跳到偏几个字符的位置。
     *
     * 所以拆开：`level`/`text` 是不可变的渲染数据；`pos` 是位置元数据，
     * 在标题序列不变时就地更新。React 拿到的还是同一批对象，不重渲染，
     * 但点击时读到的 `pos` 已经是最新的。
     */
    const emitOutline = (state: EditorState) => {
      const items = collectOutline(state)
      if (emitted && sameHeadingSequence(items, lastItems)) {
        for (let i = 0; i < items.length; i += 1) lastItems[i].pos = items[i].pos
        return
      }
      emitted = true
      lastItems = items
      onOutlineChange(items)
    }

    /**
     * 高频通道：光标所在的章节变了才碰 DOM。
     *
     * 注意它**只读** `lastItems`，不重新遍历语法树——光标每动一次就收集
     * 一遍大纲是 O(整棵树)，而结果根本不会因为移动光标而改变。
     */
    const emitActive = (state: EditorState) => {
      const index = activeIndexAt(lastItems, state.selection.main.head)
      if (index === lastActive) return
      lastActive = index
      outlineRef.current?.setActive(index)
    }

    /**
     * 每次视图更新都过一遍，但两条通道各自有判重。
     *
     * `treeChanged` 不能省：大文档打开时 Lezer 先解析视口附近，剩下的在
     * 后台按片推进，每推进一片就 `dispatch` 一个空事务。少了这个条件，
     * 大纲会永远缺后半篇，直到用户手动敲一个字符。
     */
    const syncOutline = (update: ViewUpdate) => {
      const treeChanged = syntaxTree(update.state) !== syntaxTree(update.startState)
      if (update.docChanged || treeChanged) emitOutline(update.state)
      if (update.docChanged || update.selectionSet || treeChanged) emitActive(update.state)
    }

    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: initialDoc,
        extensions: [
          lineNumbers(),
          highlightActiveLine(),
          drawSelection(),
          history(),
          keymap.of([
            // 必须排在 `defaultKeymap` 前面：同一个 keymap 里先命中先生效。
            // `preventDefault` 让 WebView 不去弹"保存网页"对话框；
            // 返回 true 同样会阻止默认行为，两个都写是为了不被后人删掉其中一个。
            {
              key: 'Mod-s',
              preventDefault: true,
              run: () => {
                saveNow()
                return true
              },
            },
            // 加粗 / 斜体：同样排在前面。`preventDefault` 在这里不是"顺手"：
            // 编辑器是一个 `contenteditable`，不拦的话浏览器会执行自己的
            // `execCommand('bold')`，往 CM6 管理的 DOM 里插 `<b>`——那是一次
            // CM6 没有发出的变更，DOMObserver 只能猜，猜错就是正文错乱。
            { key: 'Mod-b', preventDefault: true, run: (view) => applyMark(view, 'bold') },
            { key: 'Mod-i', preventDefault: true, run: (view) => applyMark(view, 'italic') },
            ...defaultKeymap,
            ...historyKeymap,
          ]),
          markdown(),
          syntaxHighlighting(kiHighlightStyle),
          markdownDecorations,
          EditorView.lineWrapping,
          EditorView.updateListener.of((update) => {
            if (update.docChanged) {
              pendingRef.current = update.state.doc.toString()
              schedule()
            }
            syncOutline(update)
          }),
        ],
      }),
    })
    viewRef.current = view

    // 挂载即发一次：新文档的大纲不能等用户敲第一个字才出现。
    // 这里不能用 `syncOutline`（它需要一个 ViewUpdate），直接走两条通道。
    emitOutline(view.state)
    emitActive(view.state)

    // 跨文件搜索跳转的落点。`revealHit` 可能在新文档挂载之前就被调用过，
    // 那时它只能把命中存下来（旧视图即将销毁，不能碰）。这里消费掉。
    //
    // 路径对不上就丢弃：用户可能在跳转途中又点了别的笔记，
    // 把上一次的命中套到新文档上，光标会落在一个毫不相干的位置。
    const pendingHit = pendingHitRef.current
    pendingHitRef.current = null
    if (pendingHit !== null && pendingHit.path === pathAtMount) {
      applyHit(view, pendingHit)
    }

    /**
     * 编辑器滚动 → 把高亮项滚进大纲面板的可视区。
     *
     * **只有这一个方向。** 反过来（面板滚动/编辑器滚动改高亮）是
     * MVP-SCOPE 点名的抖动来源：光标位置和滚动位置都想决定同一个高亮，
     * 两边轮流覆盖，用户看到的是高亮在列表里来回跳。
     *
     * 按帧合并：一次滚动会连发几十个 scroll 事件，而 DOM 操作每帧一次就够。
     */
    let scrollFrame = 0
    const onScroll = () => {
      if (scrollFrame !== 0) return
      scrollFrame = window.requestAnimationFrame(() => {
        scrollFrame = 0
        outlineRef.current?.revealActive()
      })
    }
    view.scrollDOM.addEventListener('scroll', onScroll, { passive: true })

    return () => {
      clearTimer()
      view.scrollDOM.removeEventListener('scroll', onScroll)
      if (scrollFrame !== 0) window.cancelAnimationFrame(scrollFrame)
      // 兜底：正常切换路径上上层已经取过内容并 flush 了，这里多半是空的。
      // 组件被意外卸载时（比如 HMR），至少把内容交给 Rust，而不是留在内存里。
      //
      // 路径要**在清空 ref 之前**读：换文档时 React 先跑 cleanup 再跑 effect，
      // 此刻 `pathRef` 还是这一篇的；若在 cleanup 里把它清掉再读，就只能退回
      // 挂载时的路径——改名之后那是错的。
      const content = pendingRef.current
      pendingRef.current = null
      const pathNow = ipcPath()
      if (content !== null) {
        void noteEdit(rootAtMount, pathNow, content, baseMtimeAtMount).catch(() => {
          // 组件已经卸载，没有地方展示错误了。
        })
      }
      view.destroy()
      viewRef.current = null
      pathRef.current = null
    }
    // 只在**换文档**时重建：依赖是 `docId` 而不是 `note.path`。
    // 改名会让 path 变，但撤销栈、光标、滚动位置都属于同一篇文档——
    // 用 path 做依赖等于"改个名字，撤销栈就没了"。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [docId])

  return <div className="editor" ref={hostRef} />
}
