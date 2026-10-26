import { markdown } from '@codemirror/lang-markdown'
import { EditorState } from '@codemirror/state'
import { describe, expect, it } from 'vitest'

import { activeIndexAt, collectOutline, sameHeadingSequence, type OutlineItem } from './outline'

/**
 * `outline.ts` 的行为契约。
 *
 * 这里全部用 `EditorState.create({ doc })` 构造——CM6 的 state 层是纯的，
 * 不需要 DOM，也不需要 `EditorView`。谁要是在这一层引入了 `@codemirror/view`，
 * 第一个用例就会炸成 `document is not defined`（`vitest.config.ts` 里
 * `environment: 'node'` 就是那道闸）。
 */

const stateOf = (doc: string) => EditorState.create({ doc, extensions: [markdown()] })

const outlineOf = (doc: string): OutlineItem[] => collectOutline(stateOf(doc))

const item = (level: number, pos: number, text: string): OutlineItem => ({ level, pos, text })

describe('collectOutline · 认哪些节点', () => {
  it('ATX 一到六级各出一条，level 与 # 的个数一致', () => {
    const doc = ['# 一', '## 二', '### 三', '#### 四', '##### 五', '###### 六'].join('\n\n')
    expect(outlineOf(doc).map((it) => [it.level, it.text])).toEqual([
      [1, '一'],
      [2, '二'],
      [3, '三'],
      [4, '四'],
      [5, '五'],
      [6, '六'],
    ])
  })

  it('围栏代码块里的 `#` 不是标题', () => {
    // 正则只看行首，必然把这一行当成标题——所以这里用语法树而不是正则。
    expect(outlineOf('```\n# 注释\n```\n')).toEqual([])
  })

  it('行内的 `#` 不是标题', () => {
    expect(outlineOf('见 # 号\n')).toEqual([])
  })

  it('Setext 标题：`===` 也算一级', () => {
    // 任务书里写的「只认 ATX」是简写：`HEADING_LEVEL` 刻意覆盖了 Setext 两级
    // （见 `outline.ts` 的注释），用例钉的是实现，不是简写。
    expect(outlineOf('标题\n===\n')).toEqual([item(1, 0, '标题')])
  })

  it('空文档没有标题', () => {
    expect(outlineOf('')).toEqual([])
  })
})

describe('collectOutline · 位置与文本', () => {
  it('pos 是文档偏移，不是行号', () => {
    // `## A` 在第 3 行（0 基行号 2），但它的偏移是 7。
    const items = outlineOf('intro\n\n## A\n\nbody\n\n### B\n')
    expect(items.map((it) => it.pos)).toEqual([7, 19])
  })

  it('引用块里的标题：pos 取行首而不是节点起点', () => {
    // 节点从 `>` 之后开始（from = 2），但用户眼里 `> # 标题` 是同一行。
    const items = outlineOf('> # 标题\n')
    expect(items).toEqual([item(1, 0, '标题')])
  })

  it('标题前的行首空格不算位置', () => {
    expect(outlineOf('  # 缩进标题\n')[0].pos).toBe(0)
  })

  it('闭合 ATX 的尾部 `#` 被挖掉，正文里的 `#` 保留', () => {
    expect(outlineOf('# 标题 ###\n')[0].text).toBe('标题')
    expect(outlineOf('# a # b\n')[0].text).toBe('a # b')
  })

  it('空标题：只有 `#` 时 text 是空串', () => {
    expect(outlineOf('#\n')).toEqual([item(1, 0, '')])
  })

  it('text 保留行内 Markdown 原文', () => {
    expect(outlineOf('## 有 **粗体** 的标题\n')[0].text).toBe('有 **粗体** 的标题')
  })

  it('多个标题按文档顺序返回', () => {
    expect(outlineOf('# A\n\n正文\n\n## B\n').map((it) => it.text)).toEqual(['A', 'B'])
  })
})

describe('sameHeadingSequence', () => {
  it('只看标题序列：pos 变了仍然是同一份大纲', () => {
    // 正文里打一个字，下面所有标题的 pos 都 +1。把它纳入判重，连打 100 个
    // 字符就是 100 次「大纲变了」。
    expect(
      sameHeadingSequence([item(1, 0, 'A'), item(2, 5, 'B')], [item(1, 3, 'A'), item(2, 8, 'B')]),
    ).toBe(true)
  })

  it('级别不同时判为不同', () => {
    expect(sameHeadingSequence([item(1, 0, 'A')], [item(2, 0, 'A')])).toBe(false)
  })

  it('文本不同时判为不同', () => {
    expect(sameHeadingSequence([item(1, 0, 'A')], [item(1, 0, 'A ')])).toBe(false)
  })

  it('长度不同时判为不同', () => {
    expect(sameHeadingSequence([item(1, 0, 'A')], [item(1, 0, 'A'), item(2, 4, 'B')])).toBe(false)
  })

  it('两份空大纲相同', () => {
    expect(sameHeadingSequence([], [])).toBe(true)
  })
})

describe('activeIndexAt', () => {
  it('光标在第一个标题之前时返回 -1（前言区）', () => {
    expect(activeIndexAt([item(1, 10, 'A'), item(2, 20, 'B')], 0)).toBe(-1)
  })

  it('光标正好落在标题行首时算在这一节里', () => {
    expect(activeIndexAt([item(1, 10, 'A'), item(2, 20, 'B')], 10)).toBe(0)
    expect(activeIndexAt([item(1, 10, 'A'), item(2, 20, 'B')], 20)).toBe(1)
  })

  it('光标在两个标题之间时取前一个', () => {
    expect(activeIndexAt([item(1, 10, 'A'), item(2, 20, 'B')], 15)).toBe(0)
  })

  it('光标在最后一个标题之后时取最后一个', () => {
    expect(activeIndexAt([item(1, 10, 'A'), item(2, 20, 'B')], 999)).toBe(1)
  })

  it('没有标题时返回 -1', () => {
    expect(activeIndexAt([], 42)).toBe(-1)
  })

  it('与真实大纲配合：判据是文档偏移，不是行号', () => {
    // `## A` 在偏移 4、`### B` 在偏移 14；正文「正文」在偏移 10。
    // 若把 pos 当行号用，head=10 会错落到 `### B`（第 6 行）。
    const items = outlineOf('前言\n\n## A\n\n正文\n\n### B\n\n尾\n')
    expect(items.map((it) => it.pos)).toEqual([4, 14])
    expect(activeIndexAt(items, 0)).toBe(-1)
    expect(activeIndexAt(items, 10)).toBe(0)
    expect(activeIndexAt(items, 21)).toBe(1)
  })
})
