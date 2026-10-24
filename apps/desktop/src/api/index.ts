/**
 * 前端访问 Rust 的**唯一入口**。
 *
 * 组件只准从这里 import。`@tauri-apps/*` 出现在 `api/` 之外，就是边界漏了。
 *
 * # 契约从哪来
 *
 * `bindings.ts` 由 Rust 单向生成（`pnpm bindings:generate`），**禁止手改**，
 * CI 会跑 `pnpm bindings:check` 校验 drift。本文件是它上面唯一的一层薄壳：
 * 把 `commands.vaultScan` 摊平成命名导出，组件里写的是 `vaultScan(root)`。
 *
 * 这层壳不是仪式感——它保证"命令要加前置校验 / 打点 / 统一日志"时只有一个
 * 改动点，而不是在十几个组件里各改一遍。
 */

import { commands } from './bindings'

export { commands }

export type {
  AppInfo,
  FileNode,
  KiError,
  NodeKind,
  NoteContent,
  SaveResult,
  ScanResult,
} from './bindings'

/** 递归扫描 Vault，返回 POSIX 相对路径的树。 */
export const vaultScan = commands.vaultScan

/** 读取单篇笔记。`path` 必须是 `vaultScan` 返回过的 POSIX 相对路径。 */
export const noteRead = commands.noteRead

/**
 * 原子地写入单篇笔记。
 *
 * `content` 是**完整正文**，不是增量；父目录必须已存在（不会代建）。
 * 写入是原子的，崩溃只会看到旧内容或新内容。
 *
 * M1 只提供能力，还没接编辑器——UI 入口在 M2。
 */
export const noteSave = commands.noteSave

/**
 * 健康检查：确认 Rust 侧真的起来了。
 *
 * 启动时先 ping 一次。如果这一句失败，后面所有按钮都会失败，
 * 报错信息会误导排查方向。
 */
export const appPing = commands.appPing

export { describeError, isKiError } from './errors'
export { pickFolder } from './dialog'
