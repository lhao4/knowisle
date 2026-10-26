import { useLayoutEffect, type KeyboardEvent, type MouseEvent, type RefObject } from 'react'

import type { TrashEntry } from '../api'
import { formatBytes } from '../lib/format'
import { formatDeletedAt } from '../lib/trash'

interface TrashPanelProps {
  /**
   * 面板容器。由上层持有：清空的二次确认关掉之后，焦点要回到面板上
   * （否则它落到 `<body>`，而 Esc 监听挂在面板里——用户会觉得"关不掉"）。
   */
  panelRef: RefObject<HTMLDivElement | null>
  /** 列表。空数组 = 回收站是空的（正常状态，不是错误）。 */
  entries: TrashEntry[]
  /** 正在读取列表。空列表 + 读取中 = 「读取中…」，不是「空」。 */
  loading: boolean
  /** 正在恢复的条目名（`entry`）；`null` 表示空闲。 */
  restoring: string | null
  /** 正在清空。 */
  purging: boolean
  /**
   * 列表级错误：读不出来、恢复失败、清空部分失败。
   *
   * 与"某一条的原路径未知"分开：后者是行内文案，不是错误——文件还在，
   * 只是不知道它原来在哪。
   */
  error: string | null
  /**
   * 恢复一条。传**整条**而不是 `entry` 字段：
   *
   * 恢复结果的文案要说"哪个目录没了"，而目录名只能从 `origin` 里取
   * （`a/b/c.md` → `a/b`）。返回值里没有展示数据，也不该为了一句提示
   * 去扩 IPC 契约——`origin` 本来就在这份列表里。
   */
  onRestore: (entry: TrashEntry) => void
  /** 请求清空。二次确认由上层弹（复用 `ConfirmDialog`），这里只发意图。 */
  onPurge: () => void
  onClose: () => void
}

/**
 * 回收站面板。
 *
 * # 为什么不是"侧栏里的一棵树"
 *
 * 回收站是**低频且危险**的区域：进去是为了找一篇刚删的东西，出来就该关掉。
 * 常驻在侧栏里会让它和笔记树抢注意力，而且"清空"这个动作不该一直摆在眼前。
 *
 * # 焦点
 *
 * 面板里没有输入框，焦点会在多个「恢复」按钮之间移动。两条规矩：
 *
 * 1. **挂载时把焦点移进面板**：`onKeyDown` 挂在容器上，焦点还在 body 时
 *    Esc 根本到不了这里，用户会觉得"关不掉"。
 * 2. **Tab 在面板内循环**：焦点一旦跑出去，Esc 同样失效。这里只做首尾相接，
 *    不做完整的 focus trap——面板里只有按钮，没有需要"困住"的自由输入。
 *
 * # 为什么错误行在顶部而不是逐条
 *
 * 恢复失败是**整次操作**的结果（文件被占用、条目被外部删了），挂在某一行的
 * 下面会让人以为只有那一行坏了。而"原路径未知"相反，它是**每一条**的属性，
 * 必须留在行内。
 */
export function TrashPanel({
  panelRef,
  entries,
  loading,
  restoring,
  purging,
  error,
  onRestore,
  onPurge,
  onClose,
}: TrashPanelProps) {
  useLayoutEffect(() => {
    panelRef.current?.focus()
  }, [panelRef])

  const busy = restoring !== null || purging

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault()
      if (!busy) onClose()
      return
    }
    if (e.key !== 'Tab') return

    // 首尾相接。`querySelectorAll` 每次现取：禁用态会随 `busy` 变化。
    const focusable = panelRef.current?.querySelectorAll<HTMLElement>(
      'button:not(:disabled)',
    )
    if (focusable === undefined || focusable.length === 0) return
    const first = focusable[0]
    const last = focusable[focusable.length - 1]

    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault()
      last.focus()
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault()
      first.focus()
    }
  }

  /** 点浮层外的空白处关闭。判据与其它浮层一致：`target === currentTarget`。 */
  const handleBackdrop = (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget && !busy) onClose()
  }

  return (
    <div className="trash-backdrop" onMouseDown={handleBackdrop}>
      <div
        ref={panelRef}
        className="trash-panel"
        role="dialog"
        aria-modal="true"
        aria-label="回收站"
        /*
          读取中时标 `aria-busy`：空列表 + 读取中 与 真的空 在视觉上只差一行
          文案，屏幕阅读器没有这个属性会把两者读成同一件事。顺带让"列表已经
          刷新完了"变成一个可观测的判据——验收脚本不用靠猜时间。
        */
        aria-busy={loading || undefined}
        tabIndex={-1}
        onKeyDown={handleKeyDown}
      >
        <header className="trash__head">
          <h2 className="trash__title">
            回收站
            {entries.length > 0 && <span className="trash__count">{entries.length}</span>}
          </h2>
          <button
            type="button"
            className="btn btn--small btn--danger"
            onClick={onPurge}
            disabled={busy || entries.length === 0}
            title="永久删除回收站里的所有文件"
          >
            {purging ? '清空中…' : '清空'}
          </button>
        </header>

        {error !== null && (
          <p className="trash__error" role="alert">
            {error}
          </p>
        )}

        {entries.length === 0 ? (
          <p className="trash__empty">
            {loading
              ? '读取中…'
              : '回收站是空的。删除的笔记会先放在这里，可以随时恢复。'}
          </p>
        ) : (
          <ul className="trash__list" aria-label="已删除的笔记">
            {entries.map((entry) => (
              <li key={entry.entry} className="trash__item">
                <div className="trash__main">
                  <span className="trash__name">{entry.name}</span>
                  {/*
                    原路径为 null 时必须说"不知道"。拿 `name` 拼一个 "/c.md"
                    看起来像路径，用户会以为它原本就在根目录——那是我们编的。
                  */}
                  <span
                    className={`trash__origin${entry.origin === null ? ' is-unknown' : ''}`}
                    title={entry.origin ?? undefined}
                  >
                    {entry.origin ?? '原路径未知（清单缺失）'}
                  </span>
                </div>
                <span className="trash__meta">
                  {formatBytes(entry.size)} · {formatDeletedAt(entry.deletedAt)}
                </span>
                <button
                  type="button"
                  className="btn btn--small"
                  onClick={() => onRestore(entry)}
                  disabled={busy}
                >
                  {restoring === entry.entry ? '恢复中…' : '恢复'}
                </button>
              </li>
            ))}
          </ul>
        )}

        <footer className="trash__footer">
          <span>
            恢复会回到原来的目录；同名文件已存在时自动改名为「名字 2」，不会覆盖。
          </span>
          <span className="trash__keys">Esc 关闭</span>
        </footer>
      </div>
    </div>
  )
}
