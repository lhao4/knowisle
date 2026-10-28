import { describe, expect, it } from 'vitest'

import type { IoKind, KiError } from './bindings'
import { describeError, isKiError } from './errors'

/**
 * `errors.ts` 的行为契约。
 *
 * 穷尽性本身由 `const _exhaustive: never` 兜住——这里要验的是**另一件事**：
 * 13 个变体是不是真的给出了 13 种不同的下一步。文案重复不会编译失败，
 * 只会让用户在两种完全不同的故障前看到同一句话。
 */

/** 13 个变体各一份样本。`KiError` 的判别联合由 tsc 校验，写错一个字段就编不过。 */
const SAMPLES: readonly { name: string; error: KiError; detail: string }[] = [
  { name: 'NOT_A_DIRECTORY', error: { code: 'NOT_A_DIRECTORY', detail: { path: 'notes' } }, detail: 'notes' },
  {
    name: 'NOT_A_FILE',
    error: { code: 'NOT_A_FILE', detail: { path: 'notes/子目录' } },
    detail: 'notes/子目录',
  },
  {
    name: 'PATH_ESCAPE',
    error: { code: 'PATH_ESCAPE', detail: { path: '../etc/passwd' } },
    detail: '../etc/passwd',
  },
  {
    name: 'RESERVED_PATH',
    error: { code: 'RESERVED_PATH', detail: { path: '.knowisle/index.sqlite' } },
    detail: '.knowisle/index.sqlite',
  },
  {
    name: 'IO',
    error: {
      code: 'IO',
      detail: { path: 'a.md', kind: 'PERMISSION_DENIED', message: 'os error 5' },
    },
    detail: 'a.md',
  },
  {
    name: 'INVALID_ENCODING',
    error: { code: 'INVALID_ENCODING', detail: { path: 'gbk.md' } },
    detail: 'gbk.md',
  },
  {
    name: 'ENCODING_NOT_WRITABLE',
    error: {
      code: 'ENCODING_NOT_WRITABLE',
      detail: {
        path: 'gbk.md',
        encoding: 'GBK',
        reason: '正文里有 GBK 表示不了的字符：😀。删掉它再保存。',
      },
    },
    detail: '😀',
  },
  {
    name: 'INVALID_NAME',
    error: { code: 'INVALID_NAME', detail: { name: 'CON', reason: '请用半角字符' } },
    detail: '请用半角字符',
  },
  { name: 'NAME_TAKEN', error: { code: 'NAME_TAKEN', detail: { path: 'a.md' } }, detail: 'a.md' },
  {
    name: 'TOO_LARGE',
    error: { code: 'TOO_LARGE', detail: { path: 'big.md', size: 2 * 1024 * 1024 } },
    detail: '2.0 MB',
  },
  {
    name: 'CONFLICT',
    error: {
      code: 'CONFLICT',
      detail: { path: 'a.md', expectedMtime: 1, diskMtime: 2 },
    },
    detail: 'a.md',
  },
  {
    name: 'INDEX',
    error: {
      code: 'INDEX',
      detail: { path: '.knowisle/index.sqlite', message: 'database disk image is malformed' },
    },
    detail: 'database disk image is malformed',
  },
  { name: 'INTERNAL', error: { code: 'INTERNAL', detail: '后台任务 panic' }, detail: '后台任务 panic' },
]

const io = (kind: IoKind, path = 'a.md', message = 'os error'): KiError => ({
  code: 'IO',
  detail: { path, kind, message },
})

describe('isKiError', () => {
  it('认带 `code` 的普通对象', () => {
    expect(isKiError({ code: 'INTERNAL', detail: 'x' })).toBe(true)
  })

  it('不认原型：无原型对象与类实例都算', () => {
    // 判据只是 `'code' in value`——IPC 边界上拿到的是反序列化的裸对象，
    // 依赖 `instanceof` 会让所有真实错误都漏判。
    const bare = Object.assign(Object.create(null) as object, { code: 'INTERNAL' })
    class Wrapped {
      code = 'INTERNAL'
    }
    expect(isKiError(bare)).toBe(true)
    expect(isKiError(new Wrapped())).toBe(true)
  })

  it('null / undefined / 字符串 / 数字都不是', () => {
    for (const value of [null, undefined, 'IO', 42, true]) {
      expect(isKiError(value), String(value)).toBe(false)
    }
  })

  it('没有 `code` 的对象不是', () => {
    expect(isKiError({})).toBe(false)
    expect(isKiError(new Error('boom'))).toBe(false)
  })
})

describe('describeError · KiError 的 13 个变体', () => {
  it('13 个变体映射到 13 种互不相同的文案', () => {
    const texts = SAMPLES.map((sample) => describeError(sample.error))
    expect(new Set(texts).size).toBe(SAMPLES.length)
  })

  it('每条文案都带上了该变体的关键细节（路径 / 原因 / 原文）', () => {
    for (const sample of SAMPLES) {
      expect(describeError(sample.error), sample.name).toContain(sample.detail)
    }
  })

  it('每条文案都不是空串', () => {
    for (const sample of SAMPLES) {
      expect(describeError(sample.error).length, sample.name).toBeGreaterThan(0)
    }
  })
})

describe('describeError · IO 的五种分类', () => {
  it('五种 kind 给出五种不同的下一步', () => {
    const kinds: readonly IoKind[] = ['PERMISSION_DENIED', 'BUSY', 'STORAGE_FULL', 'NOT_FOUND', 'OTHER']
    const texts = kinds.map((kind) => describeError(io(kind)))
    expect(new Set(texts).size).toBe(kinds.length)
  })

  it('权限不足指向「查权限」，占用指向「关掉别的程序」', () => {
    expect(describeError(io('PERMISSION_DENIED'))).toContain('权限')
    expect(describeError(io('BUSY'))).toContain('占用')
  })

  it('磁盘满只说「尚未落盘」，不替用户判断「内容安全」', () => {
    const text = describeError(io('STORAGE_FULL'))
    expect(text).toContain('尚未落盘')
    expect(text).not.toContain('内容安全')
  })

  it('文件不存在指向「重新扫描库」', () => {
    expect(describeError(io('NOT_FOUND'))).toContain('重新扫描')
  })

  it('OTHER 把 OS 原文当补充细节带上', () => {
    expect(describeError(io('OTHER', 'a.md', 'os error 123'))).toContain('os error 123')
  })
})

describe('describeError · 需要分支的变体', () => {
  it('CONFLICT：文件已不存在与已被外部修改是两种下一步', () => {
    const gone: KiError = {
      code: 'CONFLICT',
      detail: { path: 'a.md', expectedMtime: 1, diskMtime: null },
    }
    const changed: KiError = {
      code: 'CONFLICT',
      detail: { path: 'a.md', expectedMtime: 1, diskMtime: 2 },
    }
    expect(describeError(gone)).not.toBe(describeError(changed))
    expect(describeError(gone)).toContain('重新扫描')
    expect(describeError(changed)).toContain('重新加载')
  })

  it('INDEX 的建议是「删掉索引让它重建」，不是 IO 的「检查权限」', () => {
    // 索引是派生数据（公理 A1）：用户照 IO 的台词去做只会白忙。
    const text = describeError(SAMPLES.find((s) => s.name === 'INDEX')!.error)
    expect(text).toContain('删掉')
    expect(text).not.toContain('检查')
  })

  it('INVALID_NAME 直接展示 Rust 给的理由，不再拼一遍', () => {
    const error: KiError = { code: 'INVALID_NAME', detail: { name: 'CON', reason: '请用半角字符' } }
    expect(describeError(error)).toContain('请用半角字符')
  })

  it('INTERNAL 把 detail 原样带出', () => {
    expect(describeError({ code: 'INTERNAL', detail: '后台任务 panic' })).toContain('后台任务 panic')
  })
})

describe('describeError · 非 KiError 的输入', () => {
  it('Error 取 message', () => {
    expect(describeError(new Error('磁盘满了'))).toBe('磁盘满了')
  })

  it('字符串与数字原样转成文本', () => {
    expect(describeError('boom')).toBe('boom')
    expect(describeError(42)).toBe('42')
  })

  it('null / undefined 不抛，转成它们的字面量', () => {
    expect(describeError(null)).toBe('null')
    expect(describeError(undefined)).toBe('undefined')
  })

  it('普通对象不抛，退化成 `[object Object]`', () => {
    expect(describeError({ message: 'x' })).toBe('[object Object]')
  })
})
