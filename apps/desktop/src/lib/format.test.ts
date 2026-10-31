/**
 * `formatBytes` 的用例。
 *
 * 重点全在**边界**上：正常大小（1.5 MB）几乎不可能写错，写错的是
 * 「刚好 1024」「四舍五入到 1024」「0 / 负数 / NaN」这几种。
 *
 * 其中「永不输出 1024 KB」是回归用例：旧实现把 `1048575` 字节印成 `1024 KB`，
 * 而回收站的每一行、`TOO_LARGE` 的错误文案都走这个函数。
 *
 * 取整习惯（≥10 不留小数、<10 留一位）是**既有行为**，这里只钉住、不改它。
 */

import { describe, expect, it } from 'vitest'

import { formatBytes } from './format'

describe('formatBytes', () => {
  it('小于 1 KB 只报字节，不带小数', () => {
    expect(formatBytes(0)).toBe('0 B')
    expect(formatBytes(1)).toBe('1 B')
    expect(formatBytes(512)).toBe('512 B')
    expect(formatBytes(1023)).toBe('1023 B')
  })

  it('1024 的整数次幂各进一位', () => {
    expect(formatBytes(1024)).toBe('1.0 KB')
    expect(formatBytes(1024 ** 2)).toBe('1.0 MB')
    expect(formatBytes(1024 ** 3)).toBe('1.0 GB')
    expect(formatBytes(1024 ** 4)).toBe('1.0 TB')
  })

  it('小于 10 留一位小数，大于等于 10 不留', () => {
    expect(formatBytes(1536)).toBe('1.5 KB')
    expect(formatBytes(1024 * 512)).toBe('512 KB')
    expect(formatBytes(1024 ** 3 * 5.5)).toBe('5.5 GB')
  })

  /** 这条是它的守门人：少一次进位就会印出 `1024 KB`。 */
  it('四舍五入到 1024 时再进一位，永不输出 1024 KB', () => {
    expect(formatBytes(1024 ** 2 - 1)).toBe('1.0 MB')
    expect(formatBytes(1024 ** 3 - 1)).toBe('1.0 GB')
    expect(formatBytes(1024 * 1023.96)).not.toContain('1024 KB')
  })

  it('负数 / NaN / Infinity 一律当 0（算错的地方不该显示成真事）', () => {
    expect(formatBytes(-1)).toBe('0 B')
    expect(formatBytes(Number.NaN)).toBe('0 B')
    expect(formatBytes(Number.POSITIVE_INFINITY)).toBe('0 B')
  })

  it('超出单位表时停在 TB，不崩也不留空', () => {
    expect(formatBytes(1024 ** 5)).toBe('1024 TB')
  })
})
