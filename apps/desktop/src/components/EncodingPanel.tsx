import { useEffect, useLayoutEffect, type KeyboardEvent, type MouseEvent, type RefObject } from 'react'

import {
  describeError,
  type EncodingStats,
  type TranscodeBatch,
  type TranscodeProgress,
} from '../api'
import { buildingHint, progressPercent, summarizeBatch } from '../lib/encoding'

interface EncodingPanelProps {
  panelRef: RefObject<HTMLDivElement | null>
  /** 最近一次统计。`null` = 还没查过。 */
  stats: EncodingStats | null
  running: boolean
  /** 当前批次的真实进度（Rust 侧回报）。 */
  progress: TranscodeProgress | null
  /** 上一次批量跑完的结果。 */
  result: TranscodeBatch | null
  error: string | null
  cancelling: boolean
  /** 请求开始（弹二次确认）。 */
  onStart: () => void
  onCancel: () => void
  onClose: () => void
}

/**
 * 待转存面板。
 *
 * # 它回答的问题
 *
 * 「库里哪几篇不是 UTF-8」——在这之前，这个问题只能靠**一篇篇打开**看
 * 编辑器顶部那行提示来回答。树里没有标记，状态栏没有计数，搜索也搜不到
 * 「编码」这件事。
 *
 * # 为什么清单里必须显示编码名
 *
 * 只说「20 篇不是 UTF-8」等于把「要发生什么」藏起来一半：GBK 的篇转存之后
 * 在别的工具里也能读，Big5 的篇转存之后 Windows 记事本可能反而认不出来
 * （它默认按 GBK 猜）。用户有权在按下按钮之前知道是哪一种。
 *
 * # 关掉面板不打断转存
 *
 * 1000 篇要跑三十几秒。这个面板**不是**「关不掉」的：Esc 与点空白处都能关，
 * 批次继续跑，侧栏入口变成「转存中 N/M」。理由见 `useEncoding` 的注释。
 */
export function EncodingPanel({
  panelRef,
  stats,
  running,
  progress,
  result,
  error,
  cancelling,
  onStart,
  onCancel,
  onClose,
}: EncodingPanelProps) {
  useLayoutEffect(() => {
    panelRef.current?.focus()
  }, [panelRef])

  /**
   * 批次结束时把焦点收回面板。
   *
   * 头部那个按钮会从「取消」换成「全部转存」——React 卸载旧节点、挂新节点，
   * 焦点于是掉到 `<body>`。而 Esc 监听挂在这个容器上：焦点一出去，面板就
   * **关不掉**了（用户按 Esc 什么都不会发生，看起来像卡死）。
   *
   * 这是模态里唯一该有焦点的地方，所以直接抢回来，不需要问。
   */
  useEffect(() => {
    if (!running) panelRef.current?.focus()
  }, [running, panelRef])

  const indexState = stats?.indexState
  const building = indexState?.state === 'building'
  const total = stats?.total ?? 0
  const entries = stats?.nonUtf8 ?? []
  const percent = progressPercent(progress?.done ?? 0, progress?.total ?? 0)

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault()
      onClose()
      return
    }
    if (e.key !== 'Tab') return

    // 首尾相接，理由与回收站面板一致：焦点跑出去之后 Esc 就收不到了。
    const focusable = panelRef.current?.querySelectorAll<HTMLElement>('button:not(:disabled)')
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

  const handleBackdrop = (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget) onClose()
  }

  return (
    <div className="trash-backdrop" onMouseDown={handleBackdrop}>
      <div
        ref={panelRef}
        className="trash-panel encoding-panel"
        role="dialog"
        aria-modal="true"
        aria-label="待转存的笔记"
        aria-busy={running || undefined}
        tabIndex={-1}
        onKeyDown={handleKeyDown}
      >
        <header className="trash__head">
          <h2 className="trash__title">
            待转存
            {total > 0 && <span className="trash__count">{total}</span>}
          </h2>
          {running ? (
            <button
              type="button"
              className="btn btn--small"
              onClick={onCancel}
              disabled={cancelling}
            >
              {cancelling ? '正在停…' : '取消'}
            </button>
          ) : (
            <button
              type="button"
              className="btn btn--small"
              onClick={onStart}
              disabled={entries.length === 0}
              title="把这些笔记的磁盘编码逐篇改写成 UTF-8（原文件先备份到 .knowisle/history/）"
            >
              全部转存
            </button>
          )}
        </header>

        {error !== null && (
          <p className="trash__error" role="alert">
            {error}
          </p>
        )}

        {/*
          索引还在建时把话说清楚。没有这一行，用户看到 3 篇会以为库里只有
          3 篇——而真相是「还有 9900 篇没数到」。
        */}
        {building && indexState?.state === 'building' && (
          <p className="trash__notice" role="status">
            {buildingHint(indexState.done, indexState.total)}
          </p>
        )}

        {running ? (
          <div className="encoding__progress">
            <div className="encoding__bar" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}>
              <div className="encoding__bar-fill" style={{ width: `${percent}%` }} />
            </div>
            <p className="encoding__progress-text" role="status">
              {progress === null
                ? '正在准备…'
                : `${progress.done} / ${progress.total} 篇` +
                  (progress.failed > 0 ? `（${progress.failed} 篇失败）` : '')}
            </p>
            {progress !== null && <p className="encoding__current">{progress.path}</p>}
          </div>
        ) : result !== null ? (
          <div className="encoding__result">
            <p className="encoding__summary" role="status">
              {summarizeBatch({
                ok: result.ok.length,
                failed: result.failed.length,
                cancelled: result.cancelled,
              })}
            </p>
            {result.failed.length > 0 && (
              <ul className="encoding__failures" aria-label="转存失败的笔记">
                {result.failed.map((failure) => (
                  <li key={failure.path} className="encoding__failure">
                    <span className="encoding__path" title={failure.path}>
                      {failure.path}
                    </span>
                    <span className="encoding__reason">{describeError(failure.error)}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        ) : entries.length === 0 ? (
          <p className="trash__empty">
            {stats === null
              ? '读取中…'
              : total === 0
                ? '库里没有需要转存的笔记，每一篇都已经是 UTF-8。'
                : '清单读不出来。关掉面板再打开会重新读一次。'}
          </p>
        ) : (
          <ul className="trash__list" aria-label="待转存的笔记">
            {entries.map((entry) => (
              <li key={entry.path} className="trash__item">
                <div className="trash__main">
                  <span className="encoding__path" title={entry.path}>
                    {entry.path}
                  </span>
                </div>
                <span className="encoding__tag">{entry.encoding}</span>
              </li>
            ))}
          </ul>
        )}

        {/*
          截断必须说出来。`total` 是精确的，清单最多 1000 条——不说的话
          用户会以为「就这 1000 篇」，转完一轮才发现还有。
        */}
        {!running && result === null && stats?.truncated === true && (
          <p className="trash__notice" role="status">
            清单只列出前 {entries.length} 篇（共 {total} 篇）。转完这一批之后再打开，可以继续。
          </p>
        )}

        <footer className="trash__footer">
          <span>
            转存会把文件本身改写成 UTF-8，原文件先逐字节备份到{' '}
            <code className="confirm__path">.knowisle/history/</code>。已经是 UTF-8
            的篇不会被改动，重复点也不会多出备份。
          </span>
          <span className="trash__keys">Esc 关闭</span>
        </footer>
      </div>
    </div>
  )
}
