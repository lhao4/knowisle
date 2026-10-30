import { describe, expect, it } from 'vitest'

import type { IgnoreConfigError, IgnoreRules } from '../api'
import { configErrorLine, hasUserRules, splitBuiltin } from './ignore'

const rules = (patch: Partial<IgnoreRules> = {}): IgnoreRules => ({
  builtin: ['.git', 'logseq', 'dist', '.knowisle'],
  extra: [],
  negate: [],
  configPath: '/v/.knowisle/config.toml',
  configError: null,
  ...patch,
})

describe('splitBuiltin', () => {
  it('marks exactly the negated names', () => {
    const out = splitBuiltin(rules({ negate: ['logseq'] }))
    expect(out).toEqual([
      { name: '.git', disabled: false },
      { name: 'logseq', disabled: true },
      { name: 'dist', disabled: false },
      { name: '.knowisle', disabled: false },
    ])
  })

  it('matches case-insensitively, like the Rust side', () => {
    const out = splitBuiltin(rules({ negate: ['LOGSEQ'] }))
    expect(out.find((rule) => rule.name === 'logseq')?.disabled).toBe(true)
  })

  it('never disables anything when negate is empty', () => {
    expect(splitBuiltin(rules()).every((rule) => !rule.disabled)).toBe(true)
  })
})

describe('configErrorLine', () => {
  const error = (line: number | null): IgnoreConfigError => ({
    path: '/v/.knowisle/config.toml',
    line,
    message: 'extra 必须是字符串数组',
  })

  it('prefixes the line number when there is one', () => {
    expect(configErrorLine(error(3))).toBe('第 3 行：extra 必须是字符串数组')
  })

  it('does not invent a line number for whole-file failures', () => {
    expect(configErrorLine(error(null))).toBe('extra 必须是字符串数组')
  })
})

describe('hasUserRules', () => {
  it('is false for a bare builtin set', () => {
    expect(hasUserRules(rules())).toBe(false)
  })

  it('is true when either list has an entry', () => {
    expect(hasUserRules(rules({ extra: ['我的笔记'] }))).toBe(true)
    expect(hasUserRules(rules({ negate: ['logseq'] }))).toBe(true)
  })
})
