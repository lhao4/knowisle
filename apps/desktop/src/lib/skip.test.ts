/**
 * 跳过清单文案的用例。
 *
 * # 为什么这几条是硬约束而不是「文案随便改」
 *
 * 1. **九种原因必须九句不同的话。** 两条原因共用一句话，等于把「原因」这个
 *    字段降级成一个无信息量的标签——用户看到的仍然是「跳过 3」，只是多了一行
 *    字。验收 2 的判据就是这一条。
 * 2. **lossy 路径必须带标记。** 非 UTF-8 文件名那一档拿不到真路径，只能
 *    `to_string_lossy`。让用户照着 `U+FFFD` 去改名，等于让他去找一个不存在的
 *    文件——这是 §5 红线 6。
 * 3. **`invalidUtf8` 与 `nonUtf8Name` 的文案必须分开。** 两个名字长得像，
 *    含义完全不同（正文 vs 文件名）。文案合并会让用户改错东西。
 */

import { describe, expect, it } from 'vitest'

import type { IndexSkipReason, ScanSkipEntry, ScanSkipReason } from '../api'
import { describeIndexSkip, describeScanSkip, scanSkipPath } from './skip'

const SCAN: ScanSkipReason[] = [
  'permission',
  'nonUtf8Name',
  'notARegularFile',
  'walkError',
  'internal',
]

const INDEX: IndexSkipReason[] = ['tooLarge', 'invalidUtf8', 'io', 'unknown']

describe('扫描侧文案', () => {
  it('五种原因各自有不同的一句话', () => {
    const whats = SCAN.map((reason) => describeScanSkip(reason).what)
    expect(new Set(whats).size).toBe(SCAN.length)
  })

  it('五种原因各自有不同的一句「为什么不能自动修」', () => {
    const fixes = SCAN.map((reason) => describeScanSkip(reason).fix)
    expect(new Set(fixes).size).toBe(SCAN.length)
  })

  it('每条都同时给出 what 与 fix，且都不是空串', () => {
    for (const reason of SCAN) {
      const copy = describeScanSkip(reason)
      expect(copy.what.length, reason).toBeGreaterThan(0)
      expect(copy.fix.length, reason).toBeGreaterThan(0)
    }
  })
})

describe('索引侧文案', () => {
  it('四种原因各自有不同的一句话', () => {
    const whats = INDEX.map((reason) => describeIndexSkip(reason).what)
    expect(new Set(whats).size).toBe(INDEX.length)
  })

  it('四种原因各自有不同的一句「为什么不能自动修」', () => {
    const fixes = INDEX.map((reason) => describeIndexSkip(reason).fix)
    expect(new Set(fixes).size).toBe(INDEX.length)
  })
})

describe('两侧之间', () => {
  it('两侧的原因文案不许撞车（正文 vs 文件名是两件事）', () => {
    const scan = SCAN.map((reason) => describeScanSkip(reason).what)
    const index = INDEX.map((reason) => describeIndexSkip(reason).what)
    for (const text of index) expect(scan).not.toContain(text)
  })

  it('「正文认不出编码」明说转存帮不上（否则用户会去点那个按钮）', () => {
    expect(describeIndexSkip('invalidUtf8').fix).toContain('转存帮不上')
  })
})

describe('路径标签', () => {
  const entry = (path: string | null, pathLossy: boolean): ScanSkipEntry => ({
    path,
    pathLossy,
    reason: 'nonUtf8Name',
  })

  it('lossy 为真时把标记透出来——UI 据此加「名字含无法显示的字符」', () => {
    expect(scanSkipPath(entry('gbk-\uFFFD.md', true))).toEqual({
      text: 'gbk-\uFFFD.md',
      lossy: true,
    })
  })

  it('正常路径不带标记', () => {
    expect(scanSkipPath(entry('notes/a.md', false))).toEqual({
      text: 'notes/a.md',
      lossy: false,
    })
  })

  it('没有路径时给一句人话，不给空串', () => {
    const label = scanSkipPath(entry(null, false))
    expect(label.lossy).toBe(false)
    expect(label.text).not.toBe('')
    expect(label.text).toContain('没有路径')
  })
})
