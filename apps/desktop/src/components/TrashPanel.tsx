import { useLayoutEffect, type KeyboardEvent, type MouseEvent, type RefObject } from 'react'

import type { TrashEntry } from '../api'
import { formatBytes } from '../lib/format'
import { formatDeletedAt, TRASH_MAX_ROWS } from '../lib/trash'

interface TrashPanelProps {
  /**
   * 面板容器。由上层持有：清空的二次确认关掉之后，焦点要回到面板上
   * （否则它落到 `<body>`，而 Esc 监听挂在面板里——用户会觉得"关不掉"）。
   */
  panelRef: RefObject<HTMLDivElement | null>
  /** **已加载**的行。空数组 = 还没加载到东西（可能空，也可能在读取）。 */
  entries: TrashEntry[]
  /**
   * 回收目录里一共有多少条。
   *
   * 与 `entries.length` 分开传：分页之后前者是"一共"，后者是"加载了多少"。
   * 拿后者当总数，加载两页之后标题会写着 400 而回收站里有 10000 条。
   */
  total: number
  /**
   * 服务端说还有下一页（`next !== null`）。
   *
   * **不要**用 `entries.length < total` 推它：目录被跳过、条目在两次调用
   * 之间被恢复，都会让"这页不满"而其实还有下一页。
   */
  hasMore: boolean
  /** 正在读取第一页。空列表 + 读取中 = 「读取中…」，不是「空」。 */
  loading: boolean
  /** 正在加载下一页。按钮显示「加载中…」并禁用。 */
  loadingMore: boolean
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
  /** 成功回执（刚恢复了一条）。与 `error` 分开：那不是错误。 */
  notice: string | null
  /**
   * 恢复一条。传**整条**而不是 `entry` 字段：
   *
   * 恢复结果的文案要说"哪个目录没了"，而目录名只能从 `origin` 里取
   * （`a/b/c.md` → `a/b`）。返回值里没有展示数据，也不该为了一句提示
   * 去扩 IPC 契约——`origin` 本来就在这份列表里。
   */
  onRestore: (entry: TrashEntry) => void
  /** 加载下一页。重复点击由上层挡（`loadingMore`）。 */
  onLoadMore: () => void
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
 *
 * # 分页：底部的「已显示 N / 共 M 条」
 *
 * 这个计数不是装饰。没有它，用户看到 200 行会以为回收站里只有 200 条，
 * 而「加载更多」看起来像"再查一次"而不是"还有"。到 1000 行封顶时按钮
 * 变成一句陈述，而不是变成一个点了没反应的死按钮。
 */
export function TrashPanel({
  panelRef,
  entries,
  total,
  hasMore,
  loading,
  loadingMore,
  restoring,
  purging,
  error,
  notice,
  onRestore,
  onLoadMore,
  onPurge,
  onClose,
}: TrashPanelProps) {
  useLayoutEffect(() => {
    panelRef.current?.focus()
  }, [panelRef])

  const busy = restoring !== null || purging
  /** 到渲染上限了。不是分页大小，是 DOM 节点数的上限（见 `TRASH_MAX_ROWS`）。 */
  const atCap = entries.length >= TRASH_MAX_ROWS
  /**
   * 全都在列表里了。
   *
   * 判据是 `total`（服务端数的文件数），不是 `!hasMore`：撞到 1000 行上限时
   * 上层会把 `next` 清掉（否则按钮会一直发请求），此时 `hasMore` 也是 false，
   * 但列表里只有 1000 条而回收站里有 10000 条。拿 `!hasMore` 当"全都在"
   * 会让进度行写成「共 10000 条」，把截断藏起来。
   */
  const allLoaded = entries.length >= total

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

          注意它**不覆盖**「加载更多」：那一次读取期间 `aria-busy` 是 null，
          等它落空会立刻为真（上一刀的坑）。要等的是行数变化。
        */
        aria-busy={loading || undefined}
        tabIndex={-1}
        onKeyDown={handleKeyDown}
      >
        <header className="trash__head">
          <h2 className="trash__title">
            回收站
            {total > 0 && <span className="trash__count">{total}</span>}
          </h2>
          <button
            type="button"
            className="btn btn--small btn--danger"
            onClick={onPurge}
            disabled={busy || total === 0}
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

        {notice !== null && (
          <p className="trash__notice" role="status">
            {notice}
          </p>
        )}

        {entries.length === 0 ? (
          <p className="trash__empty">
            {loading
              ? '读取中…'
              : total === 0
                ? '回收站是空的。删除的笔记会先放在这里，可以随时恢复。'
                : '这一页读不出来。关掉面板再打开会重新读一次。'}
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

        {total > 0 && (
          <div className="trash__pager">
            <span className="trash__progress">
              {allLoaded ? `共 ${total} 条` : `已显示 ${entries.length} / 共 ${total} 条`}
            </span>
            {/*
              还有没加载的才画这个按钮。`!allLoaded` 是必要条件：正好 1000 条
              且已经全在列表里时，`hasMore` 是 false 而 `atCap` 是 true，
              只判后者会画出一个"已显示前 1000 条（共 1000 条）"的死按钮。
            */}
            {!allLoaded && (hasMore || atCap) && (
              <button
                type="button"
                className="btn btn--small"
                onClick={onLoadMore}
                disabled={loadingMore || atCap}
              >
                {atCap
                  ? `已显示前 ${TRASH_MAX_ROWS} 条（共 ${total} 条）`
                  : loadingMore
                    ? '加载中…'
                    : '加载更多'}
              </button>
            )}
          </div>
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
