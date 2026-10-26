import type { ChangeSpec, Text } from '@codemirror/state'
import { describe, expect, it } from 'vitest'

import { toggleMark, type MarkKind } from './marks'

/**
 * 单个改动。
 *
 * CM6 只导出了 `ChangeSpec`（联合）与 `ChangeSet`（内部形态），没有给这个
 * 裸对象起名字，所以这里自己写一遍——字段名与 `ChangeSpec` 的定义一致。
 */
type Change = { from: number; to?: number; insert?: string | Text }

/**
 * `marks.ts` 的行为契约。
 *
 * 这一层的价值不在于「跑得比端到端快」，而在于**可枚举**：星号段的奇偶性
 * 有四类边界（段长 1/2/3、选区在内/在外），端到端只抽了三个样本。
 * 这里逐条钉住，改错任何一格都会有一条用例红，且红的名字说得出是哪条规则。
 */

function isChangeArray(spec: ChangeSpec): spec is readonly ChangeSpec[] {
  return Array.isArray(spec)
}

/** `insert` 允许是 `Text`（CM6 的大文档优化），这里只处理 `string` 与 `Text` 两种。 */
function inserted(change: { insert?: string | Text }): string {
  if (change.insert === undefined) return ''
  return typeof change.insert === 'string' ? change.insert : change.insert.toString()
}

/**
 * 把 `ChangeSpec` 摊平成改动列表。
 *
 * `ChangeSpec` 有三种形态：单个改动、改动数组、以及 CM6 内部的 `ChangeSet`。
 * `toggleMark` 只产出前两种；`ChangeSet` 落到空列表（不是静默当成一个改动，
 * 那样会在断言里变成「什么都没改」而不是「用例写错了」）。
 */
function flatten(spec: ChangeSpec): Change[] {
  if (isChangeArray(spec)) return spec.flatMap(flatten)
  return 'from' in spec ? [spec] : []
}

/**
 * 按 CM6 的语义把改动应用到原文上。
 *
 * 从后往前改：改动区间是**按旧文档**算的，先改前面会把后面的偏移挪走。
 * 这与 `EditorState.update` 内部的处理等价，也是「一次 dispatch 传数组」
 * 能成立的前提。
 */
function apply(doc: string, spec: ChangeSpec): string {
  let out = doc
  for (const change of flatten(spec).sort((a, b) => b.from - a.from)) {
    out = out.slice(0, change.from) + inserted(change) + out.slice(change.to ?? change.from)
  }
  return out
}

/** 一次按键的结果，压成「文本 + 选区」两个值，断言里好读。 */
function press(doc: string, from: number, to: number, mark: MarkKind) {
  const edit = toggleMark(doc, from, to, mark)
  if (edit === null) return null
  return { text: apply(doc, edit.changes), anchor: edit.selection.anchor, head: edit.selection.head }
}

describe('toggleMark · 空选区', () => {
  it('空选区：插入一对定界符，光标落在两者正中间', () => {
    expect(toggleMark('', 0, 0, 'bold')).toEqual({
      changes: { from: 0, insert: '****' },
      selection: { anchor: 2, head: 2 },
    })
  })

  it('空选区：加粗插两颗星、斜体插一颗星（WIDTH 差异）', () => {
    expect(inserted(toggleMark('', 0, 0, 'bold')!.changes as Change)).toBe('****')
    expect(inserted(toggleMark('', 0, 0, 'italic')!.changes as Change)).toBe('**')
  })

  it('空选区：在空定界符正中再按一次，删掉它们并回到原位', () => {
    expect(press('****', 2, 2, 'bold')).toEqual({ text: '', anchor: 0, head: 0 })
  })

  it('空选区：斜体的一对星号只有一颗宽，正中再按也删得掉', () => {
    expect(press('**', 1, 1, 'italic')).toEqual({ text: '', anchor: 0, head: 0 })
  })

  it('空选区：光标紧挨着定界符但不在正中时，不误删而是再插一对', () => {
    // `****` 里 pos=1：左边只有一颗星，右边三颗——不构成「正中间」。
    expect(press('****', 1, 1, 'bold')).toEqual({ text: '********', anchor: 3, head: 3 })
  })

  it('空选区：只有一侧是定界符时不删', () => {
    expect(press('**', 2, 2, 'bold')).toEqual({ text: '******', anchor: 4, head: 4 })
  })

  it('空选区：只认默认字符，`__` 不算斜体的一对定界符', () => {
    // CHARS.italic 的第一项是 `*`；`_` 只在包裹时按需选用，不参与「再按删掉」。
    expect(press('__', 1, 1, 'italic')).toEqual({ text: '_**_', anchor: 2, head: 2 })
  })

  it('空选区：光标在文本中间时插入，后面的文字不动', () => {
    expect(press('ab', 1, 1, 'bold')).toEqual({ text: 'a****b', anchor: 3, head: 3 })
  })
})

describe('toggleMark · 已包裹', () => {
  it('已包裹：选区是文字时拆掉两侧定界符，选区仍是那段文字', () => {
    expect(press('**abc**', 2, 5, 'bold')).toEqual({ text: 'abc', anchor: 0, head: 3 })
  })

  it('已包裹：双击选中整段（星号在选区内）也能拆', () => {
    expect(press('**abc**', 0, 7, 'bold')).toEqual({ text: 'abc', anchor: 0, head: 3 })
  })

  it('已包裹：斜体的 `_abc_` 按一次拆掉', () => {
    expect(press('_abc_', 1, 4, 'italic')).toEqual({ text: 'abc', anchor: 0, head: 3 })
  })

  it('已包裹：斜体的 `*abc*` 按一次拆掉', () => {
    expect(press('*abc*', 1, 4, 'italic')).toEqual({ text: 'abc', anchor: 0, head: 3 })
  })

  it('已包裹：只拆一层，相邻的加粗保持原样', () => {
    // `**_abc_**` 上按斜体：拆的是 `_`，`**` 一个字都不动。
    expect(press('**_abc_**', 3, 6, 'italic')).toEqual({ text: '**abc**', anchor: 2, head: 5 })
  })
})

describe('toggleMark · 未包裹', () => {
  it('未包裹：包一层，选区跟着文字走（不把星号吞进选区）', () => {
    // head 落在收尾定界符**之前**：选区是 `abc`，不是 `abc**`。
    expect(press('abc', 0, 3, 'bold')).toEqual({ text: '**abc**', anchor: 2, head: 5 })
  })

  it('未包裹：斜体只包一颗星', () => {
    expect(press('abc', 0, 3, 'italic')).toEqual({ text: '*abc*', anchor: 1, head: 4 })
  })
})

describe('toggleMark · 反向选区', () => {
  it('反向选择：anchor 在右时结果方向不变', () => {
    const edit = toggleMark('abc', 3, 0, 'bold')!
    expect(edit.selection).toEqual({ anchor: 5, head: 2 })
    expect(apply('abc', edit.changes)).toBe('**abc**')
  })

  it('反向选择：已包裹时按原方向返回，锚点仍在右侧', () => {
    const edit = toggleMark('**abc**', 5, 2, 'bold')!
    expect(edit.selection).toEqual({ anchor: 3, head: 0 })
    expect(apply('**abc**', edit.changes)).toBe('abc')
  })
})

describe('toggleMark · 星号段的奇偶性', () => {
  it('加粗套斜体：`**abc**` 上按斜体改用下划线，不重排已有的星号', () => {
    // 若照常插 `*` 会得到 `***abc***`——三颗星在 CommonMark 里是歧义形态，
    // 下一次按键认不出哪一颗属于谁。
    expect(press('**abc**', 2, 5, 'italic')).toEqual({ text: '**_abc_**', anchor: 3, head: 6 })
  })

  it('奇偶性：`***abc***` 的星号段是奇数，按加粗不拆它，而是再包一层', () => {
    // 三颗星既可能是「加粗套斜体」也可能是「斜体套加粗」，字符串层面无法区分。
    expect(press('***abc***', 3, 6, 'bold')).toEqual({
      text: '*****abc*****',
      anchor: 5,
      head: 8,
    })
  })

  it('奇偶性：段长不足 width 时不算定界符（一颗星不能当加粗用）', () => {
    expect(press('*abc*', 1, 4, 'bold')).toEqual({ text: '***abc***', anchor: 3, head: 6 })
  })
})

describe('toggleMark · 下划线的词内限制', () => {
  it('snake_case：词内的下划线不当定界符，斜体改用星号', () => {
    // 拆掉两侧下划线会得到 `snakecasename`——一次按键毁掉一个变量名。
    expect(press('snake_case_name', 6, 10, 'italic')).toEqual({
      text: 'snake_*case*_name',
      anchor: 7,
      head: 11,
    })
  })

  it('下划线外侧是 ASCII 标识符时不拆，外侧是标点时才拆', () => {
    expect(press('foo_bar_baz', 4, 7, 'italic')!.text).toBe('foo_*bar*_baz')
    expect(press('(_abc_)', 2, 5, 'italic')!.text).toBe('(abc)')
  })
})

describe('toggleMark · 多行与空行', () => {
  it('多行选区：一行一对定界符，空行不包', () => {
    // 整体包裹会得到 `**a\n\nb**`，而段落边界会终止行内解析——按了等于没按。
    expect(press('a\n\nb', 0, 4, 'bold')).toEqual({
      text: '**a**\n\n**b**',
      anchor: 2,
      head: 10,
    })
  })

  it('多行选区：纯空白行也跳过，不多出四颗星', () => {
    expect(press('a\n   \nb', 0, 7, 'bold')!.text).toBe('**a**\n   \n**b**')
  })

  it('多行选区：改动一次给齐，是数组而不是多次 dispatch', () => {
    const edit = toggleMark('a\n\nb', 0, 4, 'bold')!
    expect(Array.isArray(edit.changes)).toBe(true)
    expect((edit.changes as readonly Change[]).length).toBe(4)
  })

  it('选区全是空行时返回 null，调用方据此不拦按键', () => {
    expect(toggleMark('\n\n', 0, 2, 'bold')).toBeNull()
  })

  it('选区全是空白字符时返回 null', () => {
    expect(toggleMark('   ', 0, 3, 'italic')).toBeNull()
  })

  it('选区跨行且全是空行时返回 null', () => {
    expect(toggleMark('a\n\n\n\nb', 2, 4, 'bold')).toBeNull()
  })
})
