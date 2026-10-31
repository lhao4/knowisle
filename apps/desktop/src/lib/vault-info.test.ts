/**
 * 库信息面板纯逻辑的用例。
 *
 * 四条状态文案各钉一条**不能少的信息**：`idle` 不能说成「空」、`building`
 * 必须说「可能不全」、`failed` 必须带上原因、`ready` 才允许说「完整」。
 * 这些句子被改坏时不会报错，只会让用户误解搜索结果的完整性。
 */

import { describe, expect, it } from 'vitest'

import { describeIndexState, formatScanMs } from './vault-info'

describe('describeIndexState', () => {
  it('idle：说清「不代表索引是空的」', () => {
    const copy = describeIndexState({ state: 'idle' })
    expect(copy.label).toBe('还没开始')
    expect(copy.detail).toContain('不代表索引是空的')
  })

  it('building：带上进度，并明说「可能不全」', () => {
    const copy = describeIndexState({ state: 'building', done: 320, total: 1000 })
    expect(copy.label).toContain('320')
    expect(copy.label).toContain('1000')
    expect(copy.detail).toContain('可能不全')
  })

  it('ready：只有这一档允许说「完整」', () => {
    expect(describeIndexState({ state: 'ready' }).detail).toContain('完整')
    expect(describeIndexState({ state: 'idle' }).detail).not.toContain('结果完整')
    expect(
      describeIndexState({ state: 'building', done: 1, total: 2 }).detail,
    ).not.toContain('结果完整')
  })

  it('failed：原因必须出现，且不许说「搜不到」', () => {
    const copy = describeIndexState({ state: 'failed', message: '磁盘满了' })
    expect(copy.label).toBe('建立失败')
    expect(copy.detail).toContain('磁盘满了')
    // 已建好的那部分仍然是对的——这句话是 failed 这一档的重点。
    expect(copy.detail).toContain('仍然是对的')
  })
})

describe('formatScanMs', () => {
  it('正常耗时四舍五入到毫秒', () => {
    expect(formatScanMs(6.4)).toBe('6 ms')
    expect(formatScanMs(44)).toBe('44 ms')
  })

  it('小于 1 ms 说「不到 1 ms」，不说 0 ms', () => {
    expect(formatScanMs(0)).toBe('不到 1 ms')
    expect(formatScanMs(0.4)).toBe('不到 1 ms')
  })

  it('负数 / NaN 说「未知」', () => {
    expect(formatScanMs(-1)).toBe('未知')
    expect(formatScanMs(Number.NaN)).toBe('未知')
  })
})
