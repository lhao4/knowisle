import { markdown } from '@codemirror/lang-markdown'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language'
import { EditorState } from '@codemirror/state'
import {
  EditorView,
  drawSelection,
  highlightActiveLine,
  keymap,
  lineNumbers,
} from '@codemirror/view'
import { tags } from '@lezer/highlight'
import { useEffect, useImperativeHandle, useRef, type Ref } from 'react'

import { describeError, noteEdit, type NoteContent, type SaveState } from '../api'

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
 */
const kiHighlightStyle = HighlightStyle.define([
  { tag: tags.heading, fontWeight: '600', color: 'var(--ki-text-primary)' },
  { tag: tags.strong, fontWeight: '600' },
  { tag: tags.emphasis, fontStyle: 'italic' },
  { tag: [tags.link, tags.url], color: 'var(--ki-accent)' },
  { tag: [tags.monospace, tags.processingInstruction], color: 'var(--ki-text-muted)' },
  { tag: tags.quote, color: 'var(--ki-text-muted)' },
  { tag: tags.list, color: 'var(--ki-accent)' },
])

export interface NoteEditorHandle {
  /** 编辑器里此刻的完整正文。切换笔记前用它把最后几个字捞出来。 */
  getContent: () => string
}

interface NoteEditorProps {
  root: string
  note: NoteContent
  ref?: Ref<NoteEditorHandle>
  onStateChange: (state: SaveState) => void
  onError: (message: string | null) => void
}

/**
 * CodeMirror 6 编辑器。
 *
 * # 三条设计约束
 *
 * 1. **文档内容不进 React state。** 每次按键都 `setState` 会让 React 重渲染整棵树，
 *    而 CM6 自己维护 `EditorState`。这里只在 ref 里持有 `EditorView`，
 *    React 完全不参与输入路径。
 * 2. **`note.path` 变化才重建编辑器。** 用 `note.content` 做依赖会导致
 *    "外部改了文件 → 内容变了 → 编辑器重建 → 光标丢失"。
 * 3. **切换笔记时的顺序由上层控制。** 合并窗口（300ms）里可能还有没发出去的字符，
 *    如果上层先 flush 再卸载，那几个字就丢了。所以这里暴露 `getContent()`，
 *    由上层「先取内容 → 发 noteEdit → flush → 再切换」。
 */
export function NoteEditor({ root, note, ref, onStateChange, onError }: NoteEditorProps) {
  const hostRef = useRef<HTMLDivElement>(null)
  const viewRef = useRef<EditorView | null>(null)
  /** 最近一次尚未发出的正文。`null` 表示没有待发内容。 */
  const pendingRef = useRef<string | null>(null)
  /** 在途的 IPC 定时器。`null` 表示当前没有等待中的合并窗口。 */
  const timerRef = useRef<number | null>(null)

  useImperativeHandle(
    ref,
    () => ({
      getContent: () => viewRef.current?.state.doc.toString() ?? pendingRef.current ?? '',
    }),
    [],
  )

  useEffect(() => {
    const host = hostRef.current
    if (host === null) return

    // 全部复制成局部常量：cleanup 跑的时候 props 与 ref 都可能已经变了，
    // 而这次清理要处理的明明是"挂载时的那一篇"。
    // 路径变化会重建整个编辑器（见下方依赖数组），所以挂载时的值就是这一篇的值。
    const rootAtMount = root
    const pathAtMount = note.path
    // 基线只在换文档时被 Rust 采信一次，所以始终传打开时的 mtime。
    const baseMtimeAtMount = note.mtime
    const initialDoc = note.content

    const flushPending = () => {
      const content = pendingRef.current
      if (content === null) return
      pendingRef.current = null
      noteEdit(rootAtMount, pathAtMount, content, baseMtimeAtMount)
        .then(onStateChange)
        .catch((err: unknown) => onError(describeError(err)))
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

    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: initialDoc,
        extensions: [
          lineNumbers(),
          highlightActiveLine(),
          drawSelection(),
          history(),
          keymap.of([...defaultKeymap, ...historyKeymap]),
          markdown(),
          syntaxHighlighting(kiHighlightStyle),
          EditorView.lineWrapping,
          EditorView.updateListener.of((update) => {
            if (!update.docChanged) return
            pendingRef.current = update.state.doc.toString()
            schedule()
          }),
        ],
      }),
    })
    viewRef.current = view

    return () => {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current)
        timerRef.current = null
      }
      // 兜底：正常切换路径上上层已经取过内容并 flush 了，这里多半是空的。
      // 组件被意外卸载时（比如 HMR），至少把内容交给 Rust，而不是留在内存里。
      const content = pendingRef.current
      pendingRef.current = null
      if (content !== null) {
        void noteEdit(rootAtMount, pathAtMount, content, baseMtimeAtMount).catch(() => {
          // 组件已经卸载，没有地方展示错误了。
        })
      }
      view.destroy()
      viewRef.current = null
    }
    // 只在切换笔记时重建：见组件文档第 2 条。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [note.path])

  return <div className="editor" ref={hostRef} />
}
