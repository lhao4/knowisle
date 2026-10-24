/** Vault 相关命令。与 Rust 侧 `commands.rs` 的 `vault_scan` / `note_read` 一一对应。 */

import { invoke } from '@tauri-apps/api/core'

import type { NoteContent, ScanResult } from './types'

/** 递归扫描 Vault，返回 POSIX 相对路径的树。 */
export const vaultScan = (root: string) =>
  invoke<ScanResult>('vault_scan', { root })

/** 读取单篇笔记。`path` 必须是 `vaultScan` 返回过的 POSIX 相对路径。 */
export const noteRead = (root: string, path: string) =>
  invoke<NoteContent>('note_read', { root, path })
