/**
 * 把 Rust 抛回来的错误翻译成人话。
 *
 * 这是 IPC 边界的一部分：`KiError` 的判别联合是契约，翻译规则必须和契约放在一起，
 * 否则 Rust 加了新错误码，改的地方会散落在各个组件里。
 *
 * `KiError` 本身来自 `bindings.ts`（Rust 生成），所以新加变体时这里的 `switch`
 * 会立刻编译失败——这正是我们要的：错误文案不能静默落后于契约。
 *
 * **这条保证靠的是「没有 `default`」**：有了 `default`，`error` 只会被收窄成
 * 「剩余成员」而不是 `never`，tsc 不会报错，新变体就静默落到兜底文案。
 * 文件末尾那行 `const _exhaustive: never = error` 是这条约束的可执行形式。
 */

import { formatBytes } from '../lib/format'
import type { KiError } from './bindings'

/** `IO` 的 detail。从契约里投影，不手抄一遍形状。 */
type IoDetail = Extract<KiError, { code: 'IO' }>['detail']

export function isKiError(value: unknown): value is KiError {
  return typeof value === 'object' && value !== null && 'code' in value
}

/**
 * `IO` 的五种可操作分类。
 *
 * 文案的差别不是修辞，而是**用户下一步动作**：改权限 / 关掉占用程序 /
 * 清磁盘 / 重新扫描 / 没有建议。五条必须互不相同——如果两条给出同一个动作，
 * 那它们就该是同一个 `kind`，Rust 侧的分类也就错了。
 */
function describeIo(detail: IoDetail): string {
  switch (detail.kind) {
    case 'PERMISSION_DENIED':
      return `没有权限访问：${detail.path}\n检查这个文件或文件夹的权限（只读、系统保护）。`
    case 'BUSY':
      return `文件正被别的程序占用：${detail.path}\n关掉打开它的程序（同步盘、杀毒、另一个编辑器）后重试。`
    case 'STORAGE_FULL':
      // 刻意不写「内容安全」：磁盘满时内容只在内存里，一次崩溃就没了。
      // 文案不能替用户下这个判断，只能说清「还没落盘」。
      return `磁盘空间不足：${detail.path}\n清理磁盘后重试；内容还在编辑器里，但尚未落盘。`
    case 'NOT_FOUND':
      return `文件已经不在磁盘上：${detail.path}\n它可能被别的程序删掉或改名了。重新扫描库后再打开。`
    case 'OTHER':
      return `读写失败：${detail.path}\n${detail.message}`
  }
  // 同 describeError：删掉任一 kind，这一行就会因 detail.kind 不再是 never 而报错。
  const _exhaustive: never = detail.kind
  void _exhaustive
}

/** 把任意异常翻译成一句人话。UI 只该展示这个结果。 */
export function describeError(error: unknown): string {
  if (!isKiError(error)) {
    return error instanceof Error ? error.message : String(error)
  }
  switch (error.code) {
    case 'NOT_A_DIRECTORY':
      return `这不是一个文件夹：${error.detail.path}`
    case 'NOT_A_FILE':
      return `这不是一个文件：${error.detail.path}\n重命名与删除只对文件生效；文件夹请在文件管理器里操作。`
    case 'PATH_ESCAPE':
      return `路径越界，已拒绝：${error.detail.path}`
    case 'RESERVED_PATH':
      return `这是应用自己的元数据（.knowisle/），不能改名或删除：${error.detail.path}`
    case 'IO':
      return describeIo(error.detail)
    case 'INVALID_ENCODING':
      return `文件不是合法的 UTF-8：${error.detail.path}`
    case 'INVALID_NAME':
      // 理由由 Rust 给出（"请用半角"这类可执行的话）。前端不再拼一遍，
      // 否则规则一旦增删，两处文案就会各说各的。
      return `文件名不合法：${error.detail.reason}`
    case 'NAME_TAKEN':
      return `已经有一个同名文件：${error.detail.path}`
    case 'TOO_LARGE':
      return `文件过大（${formatBytes(error.detail.size)}）：${error.detail.path}`
    case 'CONFLICT':
      // diskMtime 为 null = 文件在磁盘上已经没了。这两种情况下一步不同：
      // 一个是「重新加载 / 覆盖」，另一个是「它被别人删了，别写回去」。
      return error.detail.diskMtime === null
        ? `磁盘上已经没有这个文件了：${error.detail.path}\n它被别的程序删除或改名了，保存前请重新扫描库。`
        : `文件已被外部修改：${error.detail.path}\n磁盘上的版本比你打开的更新。重新加载后再改，或覆盖它。`
    case 'INDEX':
      // 索引是派生数据（公理 A1），所以这里的建议是「删掉它」而不是「修好它」。
      // 不要写成「请检查权限」——那是 IO 的台词，用户照做只会白忙。
      return `搜索索引不可用：${error.detail.path}\n删掉这个文件后重新扫描库，索引会自动重建。\n${error.detail.message}`
    case 'INTERNAL':
      return `内部错误：${error.detail}`
  }
  // 兜底不可达。**不要**把它换成 `default:` 分支——那样下面这行就永远成立，
  // 穷尽性检查也就没了。
  const _exhaustive: never = error
  void _exhaustive
}
