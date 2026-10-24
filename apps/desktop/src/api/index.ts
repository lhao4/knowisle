/**
 * 前端访问 Rust 的**唯一入口**。
 *
 * 组件只准从这里 import。`@tauri-apps/*` 出现在 `api/` 之外，就是边界漏了。
 *
 * 这条约束的价值在于：M1 换成 `tauri-specta` 生成的 `bindings.ts` 时，
 * 只需要改这个目录，组件一行都不用动。
 */

export type {
  AppInfo,
  FileNode,
  KiError,
  NodeKind,
  NoteContent,
  ScanResult,
} from './types'

export { describeError, isKiError } from './errors'

export { noteRead, vaultScan } from './vault'
export { appPing } from './system'
export { pickFolder } from './dialog'
