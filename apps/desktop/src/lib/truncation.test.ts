/**
 * 截断横幅文案的用例。
 *
 * # 为什么这几条是硬约束
 *
 * 1. **不许有硬编码数字。** 横幅曾经写死 20000，默认值改了它继续撒谎——
 *    这是它已经错过一次的原因。用例把「文案里的每个数字都必须等于入参」
 *    钉死，改成常量会立刻红。
 * 2. **「条目」与「篇」必须同时出现。** 上限数的是条目（目录 + 文件），
 *    撞上限时能看到的笔记数恒不超过上限。只说「篇」就是在重复 B-06 的错。
 * 3. **必须说出边界目录可能显示为空。** 这是截断的真实副作用（判据 6 的
 *    Rust 用例钉住了树的那一半），不说就等于让用户以为笔记丢了。
 */

import { describe, expect, it } from 'vitest'

import { truncationNotice } from './truncation'

describe('截断横幅', () => {
  it('数字只来自入参：3 与 20000 各自出现在文案里', () => {
    expect(truncationNotice(3)).toContain('3')
    expect(truncationNotice(20_000)).toContain('20000')
    expect(truncationNotice(3)).not.toContain('20000')
  })

  it('上限变了，文案跟着变', () => {
    expect(truncationNotice(3)).not.toBe(truncationNotice(20_000))
  })

  it('文案里没有任何硬编码数字', () => {
    for (const limit of [3, 20_000]) {
      const numbers = truncationNotice(limit).match(/\d+/g) ?? []
      expect(numbers.length, `limit=${limit}`).toBeGreaterThan(0)
      expect(new Set(numbers), `limit=${limit}`).toEqual(new Set([String(limit)]))
    }
  })

  it('同时说清「条目」与「篇」的区别', () => {
    for (const limit of [3, 20_000]) {
      const notice = truncationNotice(limit)
      expect(notice, `limit=${limit}`).toContain('条目')
      expect(notice, `limit=${limit}`).toContain('篇')
      expect(notice, `limit=${limit}`).toContain('目录')
      expect(notice, `limit=${limit}`).toContain('不超过')
    }
  })

  it('说出「被截断的位置上，目录可能显示为空」', () => {
    const notice = truncationNotice(20_000)
    expect(notice).toContain('显示为空')
    expect(notice).toContain('磁盘上')
  })
})
