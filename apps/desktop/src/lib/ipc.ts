/**
 * IPC 契约（手写版）。
 *
 * M0 阶段只有 3 个命令，手写类型是划算的。
 * M1 引入 `tauri-specta` 后，这个文件会被从 Rust 单向生成的
 * `bindings.ts` 取代 —— 届时**禁止手改**，CI 会校验 drift。
 */
import { invoke } from '@tauri-apps/api/core'

/* ───────────────────────── 数据结构 ───────────────────────── */

export type NodeKind = 'dir' | 'file'

/**
 * Vault 树节点。
 *
 * `path` 永远是 POSIX 相对路径（`/` 分隔），由 Rust 产出。
 * 前端**不得**自行拼接路径 —— 这是 Windows 上树结构分裂的头号原因。
 */
export interface FileNode {
  name: string
  path: string
  kind: NodeKind
  size: number
  mtime: number
  children?: FileNode[]
}

export interface ScanResult {
  root: string
  tree: FileNode
  totalFiles: number
  totalDirs: number
  truncated: boolean
  skipped: number
  elapsedMs: number
}

export interface NoteContent {
  path: string
  content: string
  size: number
  mtime: number
}

/* ───────────────────────── 错误模型 ───────────────────────── */

/**
 * 与 Rust 侧 `KiError` 的 adjacently tagged 序列化一一对应：
 * `{ "code": "IO", "detail": { "path": "...", "message": "..." } }`
 *
 * 用 `code` 分支，**永远不要**解析 message 字符串。
 */
export type KiError =
  | { code: 'NOT_A_DIRECTORY'; detail: { path: string } }
  | { code: 'PATH_ESCAPE'; detail: { path: string } }
  | { code: 'IO'; detail: { path: string; message: string } }
  | { code: 'INVALID_ENCODING'; detail: { path: string } }
  | { code: 'TOO_LARGE'; detail: { path: string; size: number } }
  | { code: 'INTERNAL'; detail: string }

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

/* ───────────────────────── 工具函数 ───────────────────────── */

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ['KB', 'MB', 'GB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unit]}`
}

/* ───────────────────────── 命令封装 ───────────────────────── */

export const vaultScan = (root: string) =>
  invoke<ScanResult>('vault_scan', { root })

export const noteRead = (root: string, path: string) =>
  invoke<NoteContent>('note_read', { root, path })

export const appPing = () =>
  invoke<{ name: string; version: string; pid: number }>('app_ping')
