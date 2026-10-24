/** 应用级命令。与 Rust 侧 `commands.rs` 的 `app_ping` 对应。 */

import { invoke } from '@tauri-apps/api/core'

import type { AppInfo } from './types'

/**
 * 健康检查：确认 Rust 侧真的起来了。
 *
 * 启动时先 ping 一次。如果这一句失败，后面所有按钮都会失败，
 * 报错信息会误导排查方向。
 */
export const appPing = () => invoke<AppInfo>('app_ping')
