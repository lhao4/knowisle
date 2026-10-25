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
  SaveState,
  ScanResult,
  SearchHit,
  SearchResult,
} from './bindings'

/** 递归扫描 Vault，返回 POSIX 相对路径的树。 */
export const vaultScan = commands.vaultScan

/**
 * 全文搜索：ASCII 大小写折叠后的**子串**匹配，不模糊、不分词、不支持正则。
 *
 * `refreshPath` 传当前正在编辑的笔记路径：Rust 会把它从磁盘重读一次，
 * 让刚打的字立刻可见。其余文件不重读。
 *
 * 调用方必须**先** flush 当前笔记（`noteEdit` → `noteFlush`），
 * 否则磁盘上还没有刚打的字，搜索自然找不到——那不是搜索的 bug。
 */
export const vaultSearch = commands.vaultSearch

/** 读取单篇笔记。`path` 必须是 `vaultScan` 返回过的 POSIX 相对路径。 */
export const noteRead = commands.noteRead

/**
 * 原子地写入单篇笔记。
 *
 * `content` 是**完整正文**，不是增量；父目录必须已存在（不会代建）。
 * 写入是原子的，崩溃只会看到旧内容或新内容。
 *
 * 这是"无条件覆盖"：不做冲突检查。编辑器请用 {@link noteEdit}。
 */
export const noteSave = commands.noteSave

/**
 * 新建一篇空笔记，返回它的 Vault 相对路径。
 *
 * `dir` 是 Vault 相对目录（空串表示根目录），必须已存在——不会代建目录。
 * `name` 没有 `.md` / `.markdown` 后缀会自动补 `.md`。
 *
 * 文件名合法性**只在 Rust 侧判定**（`ki_vault::naming`）。前端可以提前给
 * 一句提示，但那只是即时反馈，不是闸门——第二个入口（将来的 `isle` CLI）
 * 走的是同一份 Rust 规则。
 *
 * 撞名返回 `NAME_TAKEN`，原文件一个字节都不动。
 */
export const noteCreate = commands.noteCreate

/**
 * 重命名一篇笔记（同目录内改名，不移动、不删除），返回新的相对路径。
 *
 * 调用前必须先 `noteEdit` + `noteFlush` 把编辑器 300ms 合并窗口里的字交出去，
 * 但那**不能替代** Rust 侧的 drain：前端 flush 返回时自动保存的 worker
 * 可能还没退出，而它会把内容写到已经不存在的旧路径上（旧文件复活）。
 * 顺序由 `note_rename` 自己在 Rust 侧保证。
 *
 * 目标已存在返回 `NAME_TAKEN`（只改大小写除外——那是同一个文件）。
 */
export const noteRename = commands.noteRename

/**
 * 删除一篇笔记：**移进 `.knowisle/trash/`**，返回它在回收目录里的相对路径。
 *
 * 调用前必须先 `noteEdit` + `noteFlush`（当前笔记时），否则 trash 里会是旧版本。
 * 但那**不能替代** Rust 侧的 drain：前端 flush 返回时自动保存的 worker 可能
 * 还没退出。顺序由 `note_delete` 自己在 Rust 侧保证：
 * drain → 搬进 trash → 清空槽位。
 *
 * 冲突时拒绝删除（`CONFLICT`）：此刻用户还有「覆盖 / 放弃」两个选择，
 * 删掉就只剩一个。
 *
 * # 这不是回收站
 *
 * 应用里没有恢复入口，也没有"清空回收站"。文件还在磁盘上，要找回得自己去
 * 文件夹翻——UI 文案必须说清这一点，不能只说"移到回收站"。
 */
export const noteDelete = commands.noteDelete

/**
 * 提交一次编辑，由 Rust 侧的节流器决定何时落盘（1.5 秒）。
 *
 * `baseMtime` 传打开这篇笔记时 `noteRead` 返回的 mtime，
 * 只在切换文档时被采信一次——基线随后由 Rust 随每次成功落盘向前推进。
 *
 * 返回 {@link SaveState}：是否有内容未落盘、最近一次落盘的 mtime、最近一次错误。
 */
export const noteEdit = commands.noteEdit

/** 立刻落盘，不等节流周期。切换笔记、关窗前调用。 */
export const noteFlush = commands.noteFlush

/** 查询保存状态，不触发写入。只在前端 `dirty` 为真时轮询。 */
export const noteStatus = commands.noteStatus

/**
 * 健康检查：确认 Rust 侧真的起来了。
 *
 * 启动时先 ping 一次。如果这一句失败，后面所有按钮都会失败，
 * 报错信息会误导排查方向。
 */
export const appPing = commands.appPing

export { describeError, isKiError } from './errors'
export { pickFolder } from './dialog'
