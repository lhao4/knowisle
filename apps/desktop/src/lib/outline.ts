/**
 * 从 **EditorState 的语法树**派生大纲。
 *
 * # 为什么不从 `note.content` 重新解析
 *
 * 编辑器里已经有一棵由 Lezer 增量维护的语法树。再拿字符串跑一遍解析器，
 * 等于同一份真相维护两份：光标刚敲下的那个字符，在字符串里是新的、在树里
 * 可能还是旧的（增量解析有延迟），两份数据会在某一帧里打架，表现为
 * 「大纲里的标题和正文对不上」。而且整篇重新解析是 O(n)，每次按键都做一次。
 *
 * # 为什么不用正则
 *
 * `# 标题` 出现在代码块里、引用块里、行内代码里都不算标题。正则只看行首，
 * 必然把 ```\n# 注释\n``` 里的那行当成标题。语法树已经把这些区分好了——
 * 我们只做一件事：认出节点名。
 */

import { syntaxTree } from '@codemirror/language'
import type { EditorState } from '@codemirror/state'

export interface OutlineItem {
  /** 1–6。Setext 只有两级，其余级别由 ATX 提供。 */
  level: number
  /**
   * 标题**所在行的行首**位置。
   *
   * 用行首而不是节点起点：`> # 标题` 的节点起点在 `>` 之后，光标停在 `>` 上
   * 时它就不该算「在这一节里」——用户眼里那就是同一行。
   */
  pos: number
  /** 标题正文（原始 Markdown，逐字）。过长由 CSS 省略号处理。 */
  text: string
}

/**
 * 节点名 → 级别。
 *
 * 用查表而不是 `/^(?:ATX|Setext)Heading([1-6])$/`：这是**节点名分派**，
 * 不是文本解析，写死 8 个键比一条正则更容易一眼确认覆盖了哪些节点。
 */
const HEADING_LEVEL: Readonly<Record<string, number>> = {
  ATXHeading1: 1,
  ATXHeading2: 2,
  ATXHeading3: 3,
  ATXHeading4: 4,
  ATXHeading5: 5,
  ATXHeading6: 6,
  SetextHeading1: 1,
  SetextHeading2: 2,
}

/**
 * 遍历整棵树收集标题，按文档顺序返回。
 *
 * 复杂度是 O(节点数)，与**文档长度**同阶而不是与标题数同阶。10 万字文档
 * 约 4 万个节点，实测一次 1ms 量级（见 `docs/M2-OUTLINE.md` 的测量）。
 * 调用方只在 `docChanged` / 语法树换代时调它，不在每次滚动时调。
 */
export function collectOutline(state: EditorState): OutlineItem[] {
  const items: OutlineItem[] = []
  const { doc } = state

  syntaxTree(state).iterate({
    enter: (node) => {
      const level = HEADING_LEVEL[node.name]
      if (level === undefined) return

      let text = ''
      // **不能遍历子节点拼文本。** Lezer 的纯文本不是节点——`# 标题` 里
      // 除了 `HeaderMark` 之外没有任何子节点，按子节点拼出来是空字符串
      // （第一版就是这么错的，验收 1 立刻抓到）。
      //
      // 正确的做法是反过来：先取整个标题的原文，再把 `HeaderMark` 的区间
      // 挖掉。ATX 的 `#`（含闭合 ATX 结尾那串）、Setext 的 `===` 都是
      // HeaderMark，`getChildren` 只返回直接子节点，不会误伤正文里的 `#`。
      const heading = node.node
      let cursor = heading.from
      for (const mark of heading.getChildren('HeaderMark')) {
        text += doc.sliceString(cursor, mark.from)
        cursor = mark.to
      }
      text += doc.sliceString(cursor, heading.to)

      items.push({
        level,
        pos: doc.lineAt(node.from).from,
        text: text.trim(),
      })
    },
  })

  return items
}

/**
 * 两份大纲的**标题序列**是否相同（级别 + 文本，**不看 `pos`**）。
 *
 * # 为什么必须忽略 `pos`
 *
 * `pos` 是绝对文档偏移：在正文里打一个字，它下面所有标题的 `pos` 都 +1。
 * 若把 `pos` 纳入判重，连打 100 个字符就是 100 次「大纲变了」——
 * React 渲染次数立刻与按键次数同阶，这正是本功能要避免的。
 *
 * 大纲**显示什么**只由标题序列决定；`pos` 是「跳到哪」的位置元数据，
 * 由调用方就地更新（见 `NoteEditor` 的 `emitOutline`）。
 *
 * 逐项比而不是算字符串指纹：500 项时不用分配任何中间字符串。
 */
export function sameHeadingSequence(a: OutlineItem[], b: OutlineItem[]): boolean {
  if (a.length !== b.length) return false
  for (let i = 0; i < a.length; i += 1) {
    if (a[i].level !== b[i].level || a[i].text !== b[i].text) return false
  }
  return true
}

/**
 * 光标位置 → 所在章节的下标。取 `pos <= head` 的**最后一个**标题。
 *
 * 返回 `-1` 表示光标在第一个标题之前（前言区），此时不高亮任何项。
 *
 * 二分而不是从后往前扫：500 个标题的文档里，光标每动一次就要算一次，
 * 而这是高频路径（每次按键、每次点击都会走到）。9 次比较 vs 500 次。
 */
export function activeIndexAt(items: OutlineItem[], head: number): number {
  let lo = 0
  let hi = items.length - 1
  let found = -1

  while (lo <= hi) {
    const mid = (lo + hi) >> 1
    if (items[mid].pos <= head) {
      found = mid
      lo = mid + 1
    } else {
      hi = mid - 1
    }
  }

  return found
}
