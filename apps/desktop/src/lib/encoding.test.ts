import { describe, expect, it } from 'vitest'

import { buildingHint, progressPercent, summarizeBatch } from './encoding'

/**
 * 结果文案与进度折算的边界。
 *
 * 这三条判据的共同点：**看起来都像"随便写一句就行"，而错的那几种写法
 * 都会让用户误解自己的处境**——把取消写成"一篇都没转"、把全失败写成
 * "部分失败"、把 0/0 写成 NaN 让进度条消失。
 */
describe('summarizeBatch', () => {
  it('全部成功时说全部', () => {
    expect(summarizeBatch({ ok: 20, failed: 0, cancelled: false })).toBe('20 篇全部转存为 UTF-8')
  })

  it('部分失败时两个数都在', () => {
    expect(summarizeBatch({ ok: 17, failed: 3, cancelled: false })).toBe('17 篇成功 / 3 篇失败')
  })

  /**
   * 全失败不能退化成「0 篇成功 / 20 篇失败」——那个 0 会被读成
   * 「大部分成功了，只有几篇挂了」的反面，而实际处境是「一篇都没成，
   * 去查权限」。
   */
  it('全失败时不提成功数', () => {
    expect(summarizeBatch({ ok: 0, failed: 20, cancelled: false })).toBe('20 篇全部失败')
  })

  /**
   * 取消**不是**失败：已经转好的那些是实打实的成果。写成「已取消」三个字
   * 会让用户以为白跑了三十秒。
   */
  it('取消时把已经转好的篇数说出来', () => {
    expect(summarizeBatch({ ok: 300, failed: 0, cancelled: true })).toBe(
      '已取消：300 篇已经转存为 UTF-8',
    )
    expect(summarizeBatch({ ok: 300, failed: 2, cancelled: true })).toBe(
      '已取消：300 篇已转存，2 篇失败',
    )
  })

  it('取消时一篇都没跑完也要说清楚', () => {
    expect(summarizeBatch({ ok: 0, failed: 0, cancelled: true })).toBe('已取消，一篇都没有转存')
  })
})

describe('progressPercent', () => {
  it('常规折算并四舍五入', () => {
    expect(progressPercent(0, 1000)).toBe(0)
    expect(progressPercent(1, 3)).toBe(33)
    expect(progressPercent(2, 3)).toBe(67)
    expect(progressPercent(1000, 1000)).toBe(100)
  })

  /**
   * `total` 为 0 时 `done / total` 是 `NaN`，而 `width: NaN%` 会被浏览器
   * 丢掉——进度条看起来像"消失了"，而不是"还没开始"。
   */
  it('0 篇时是 0 而不是 NaN', () => {
    expect(progressPercent(0, 0)).toBe(0)
    expect(progressPercent(5, 0)).toBe(0)
  })

  it('越界的输入被钳住', () => {
    expect(progressPercent(1200, 1000)).toBe(100)
    expect(progressPercent(-3, 1000)).toBe(0)
    expect(progressPercent(Number.POSITIVE_INFINITY, 1000)).toBe(0)
  })
})

describe('buildingHint', () => {
  it('带上真实进度', () => {
    expect(buildingHint(300, 1000)).toBe('索引正在建立（300/1000），这个数还会变。')
  })

  /** 分母未知（后台还没报总量）时不能写成「（0/0）」。 */
  it('总量未知时不编一个分数', () => {
    expect(buildingHint(0, 0)).toBe('索引正在建立，这个数还会变。')
  })
})
