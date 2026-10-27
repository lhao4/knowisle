/**
 * Vault 相对路径的小工具。
 *
 * # 为什么单独一个文件
 *
 * `parentDir` 同时被两个域用到：侧栏（右键菜单算「新建到哪个目录」、`createDir`）
 * 与回收站（恢复结果的文案要说"原目录是哪一级"）。放在任一 hook 里都会让另一个
 * 域 import 它——那正是这一刀要拆掉的耦合。按"正统"它该在 `lib/` 下，但本刀的
 * 改动面被限制在 `App.tsx` + `hooks/*`，所以先落在这里；等下一次动 `lib/` 时
 * 顺手搬过去，行为不变。
 */

/** 取 Vault 相对路径的父目录（POSIX 分隔符）。根目录返回空串。 */
export function parentDir(path: string): string {
  const index = path.lastIndexOf('/')
  return index < 0 ? '' : path.slice(0, index)
}
