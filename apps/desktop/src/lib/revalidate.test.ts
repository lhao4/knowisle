import { describe, expect, it } from 'vitest'

import { revalidate, type RevalidateInput } from './revalidate'

/**
 * `revalidate.ts` 的行为契约。
 *
 * 每个用例都写成「一次真实的场景」而不是「一个分支」：这段逻辑的失败方式
 * 只有两种，而两种都很贵——误报（用户每切一次窗口被问一次）与漏报
 * （用户在旧内容上继续写，直到撞一个他看不懂的冲突）。所以用例名直接写场景。
 */

const input = (over: Partial<RevalidateInput> = {}): RevalidateInput => ({
  editorContent: 'A',
  diskContent: 'A',
  diskMtime: 1000,
  baselineMtime: 1000,
  dirty: false,
  ...over,
})

describe('revalidate · 没有分叉', () => {
  it('mtime 与内容都没变 → noop（最常见的路径：切出去又切回来）', () => {
    expect(revalidate(input())).toEqual({ kind: 'noop' })
  })

  it('用户刚敲了字、合并窗口还没发出去（dirty，mtime 没动）→ noop，不许误报成外部改动', () => {
    // 这是最容易写错的一条：内容不同是「用户自己的字」，不是别人的。
    const verdict = revalidate(
      input({ editorContent: 'A 刚敲的字', diskContent: 'A', dirty: true }),
    )
    expect(verdict).toEqual({ kind: 'noop' })
  })

  it('空笔记也走同一条判据', () => {
    expect(revalidate(input({ editorContent: '', diskContent: '' }))).toEqual({ kind: 'noop' })
  })
})

describe('revalidate · 内容相同、mtime 变了（不误报，但要推基线）', () => {
  it('外部工具原样重存 → adopt-baseline，且推进到**磁盘的新 mtime**', () => {
    // 漏了这一档的两种写法都会出事：返回 noop → 下一次保存假冲突；
    // 返回 prompt → 用户什么都没改却被问「要不要重新加载」。
    expect(revalidate(input({ diskMtime: 2000 }))).toEqual({
      kind: 'adopt-baseline',
      mtime: 2000,
    })
  })

  it('外部把文件写成与编辑器完全一样的内容（§4-7）→ adopt-baseline，不弹横幅', () => {
    const verdict = revalidate(
      input({ editorContent: '# 甲\n\n正文\n', diskContent: '# 甲\n\n正文\n', diskMtime: 2000 }),
    )
    expect(verdict.kind).toBe('adopt-baseline')
  })

  it('dirty 也照样推基线：待写内容与磁盘一致时，基线不前进就是下一次假冲突', () => {
    // 用户敲了又删回原样，槽位里压着一份与磁盘相同的正文。
    // 纯函数只看得到「内容相同」，是否真的能推由 Rust 侧按 pending 判定。
    expect(revalidate(input({ diskMtime: 2000, dirty: true }))).toEqual({
      kind: 'adopt-baseline',
      mtime: 2000,
    })
  })
})

describe('revalidate · 真的分叉', () => {
  it('外部改了内容、本地没有未落盘改动 → prompt(false)', () => {
    expect(revalidate(input({ diskContent: 'B', diskMtime: 2000 }))).toEqual({
      kind: 'prompt',
      dirty: false,
    })
  })

  it('外部改了内容、本地也有未落盘改动 → prompt(true)（文案与二次确认都跟着它走）', () => {
    expect(
      revalidate(input({ editorContent: 'A 我的字', diskContent: 'B', diskMtime: 2000, dirty: true })),
    ).toEqual({ kind: 'prompt', dirty: true })
  })

  it('外部写入落在同一毫秒（mtime 没动、内容不同、本地干净）→ 靠内容兜底报出来', () => {
    // Windows 的 mtime 粒度实测够粗，只看 mtime 会漏掉这一档。
    expect(revalidate(input({ diskContent: 'B', dirty: false }))).toEqual({
      kind: 'prompt',
      dirty: false,
    })
  })

  it('只差一个尾随换行也算分叉（不做空白规范化）', () => {
    // 规范化会把「外部只改了行尾」判成相同，然后在保存时把对方的行尾抹掉。
    expect(revalidate(input({ diskContent: 'A\n', diskMtime: 2000 })).kind).toBe('prompt')
  })

  it('任何情况下都不会返回「自动重载」——判据里根本没有这一档', () => {
    const verdicts = [
      revalidate(input()),
      revalidate(input({ diskMtime: 2000 })),
      revalidate(input({ diskContent: 'B', diskMtime: 2000 })),
      revalidate(input({ diskContent: 'B', diskMtime: 2000, dirty: true })),
    ]
    expect(verdicts.map((v) => v.kind)).toEqual(['noop', 'adopt-baseline', 'prompt', 'prompt'])
  })
})
