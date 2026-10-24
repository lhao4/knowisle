/**
 * IPC 契约类型。
 *
 * 这些类型与 Rust 侧 `ki_core` 的 `Serialize` 结构一一对应，是前后端唯一共享的词汇表。
 * 参数与返回类型只在这里定义一次，组件不得自行复述。
 *
 * M1 引入 `tauri-specta` 后，本文件会被从 Rust 单向生成的 `bindings.ts` 取代——
 * 届时**禁止手改**，CI 会校验 drift。
 */

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

/** `vault_scan` 的返回值。 */
export interface ScanResult {
  root: string
  tree: FileNode
  totalFiles: number
  totalDirs: number
  truncated: boolean
  skipped: number
  elapsedMs: number
}

/** `note_read` 的返回值。 */
export interface NoteContent {
  path: string
  content: string
  size: number
  mtime: number
}

/** `app_ping` 的返回值。 */
export interface AppInfo {
  name: string
  version: string
  pid: number
}

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
