/**
 * 把 Rust 抛回来的错误翻译成人话。
 *
 * 这是 IPC 边界的一部分：`KiError` 的判别联合是契约，翻译规则必须和契约放在一起，
 * 否则 Rust 加了新错误码，改的地方会散落在各个组件里。
 */

import { formatBytes } from '../lib/format'
import type { KiError } from './types'

export function isKiError(value: unknown): value is KiError {
  return typeof value === 'object' && value !== null && 'code' in value
}

/** 把任意异常翻译成一句人话。UI 只该展示这个结果。 */
export function describeError(error: unknown): string {
  if (!isKiError(error)) {
    return error instanceof Error ? error.message : String(error)
  }
  switch (error.code) {
    case 'NOT_A_DIRECTORY':
      return `这不是一个文件夹：${error.detail.path}`
    case 'PATH_ESCAPE':
      return `路径越界，已拒绝：${error.detail.path}`
    case 'IO':
      return `读写失败：${error.detail.path}\n${error.detail.message}`
    case 'INVALID_ENCODING':
      return `文件不是合法的 UTF-8：${error.detail.path}`
    case 'TOO_LARGE':
      return `文件过大（${formatBytes(error.detail.size)}）：${error.detail.path}`
    case 'INTERNAL':
      return `内部错误：${error.detail}`
    default:
      // 类型穷尽后仍走到这里，说明 Rust 加了新错误码但前端没跟上
      return `未知错误：${JSON.stringify(error)}`
  }
}
