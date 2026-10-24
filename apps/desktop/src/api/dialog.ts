/**
 * 文件夹选择对话框。
 *
 * 走的是 `@tauri-apps/plugin-dialog` 而不是 `invoke`，但它同样是一次跨 IPC 边界的
 * 调用（`plugin:dialog|open`），所以照样收在这一层——组件不该认识任何 Tauri SDK。
 */

import { open } from '@tauri-apps/plugin-dialog'

/**
 * 弹出文件夹选择器，返回绝对路径；用户取消时返回 `null`。
 *
 * 取消**不是错误**，调用方不该把它当异常处理。
 * `title` 由调用方给：UI 文案属于 UI 层，不塞进边界层。
 */
export async function pickFolder(title: string): Promise<string | null> {
  const selected = await open({ directory: true, multiple: false, title })
  return typeof selected === 'string' ? selected : null
}
