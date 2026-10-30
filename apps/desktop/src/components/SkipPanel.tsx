import { useEffect, useLayoutEffect, type KeyboardEvent, type MouseEvent, type RefObject } from 'react'

import type { IndexSkipStats, ScanSkipEntry } from '../api'
import { buildingHint } from '../lib/encoding'
import { describeIndexSkip, describeScanSkip, scanSkipPath } from '../lib/skip'

interface SkipPanelProps {
  panelRef: RefObject<HTMLDivElement | null>
  /** 扫描侧：**精确**总数（含那些不进清单的 `Internal`）。 */
  scanSkipped: number
  scanEntries: ScanSkipEntry[]
  scanTruncated: boolean
  /** 索引侧统计。`null` = 还没查过。 */
  indexStats: IndexSkipStats | null
  error: string | null
  onClose: () => void
}

/**
 * 跳过清单面板：**两栏，两个数，互不相加**。
 *
 * # 它回答的问题
 *
 * 「跳过 3」这句话此前在三个地方意味着三件不同的事，而用户一个也查不了：
 * 一个目录没权限、一个文件名是 GBK 编的、一个 FIFO，全记成 1；索引侧一篇
 * 认不出编码、一篇超过 2MB、一篇 IO 失败，也全记成 1。两处还显示同一个词，
 * 于是「侧栏说跳过 3、搜索说跳过 1」看起来像 bug。
 *
 * # 为什么是两栏而不是一个合并的清单
 *
 * 两个数**任何时候不相等也合法**：一篇 3MB 的 `.md` 在扫描侧是一棵完全正常
 * 的树节点，只有索引侧才跳过它；反过来，一个没有读权限的目录在扫描侧计一条，
 * 索引侧根本看不见它。合成一个数（相加 / 取最大 / 只显示其中一个）之后，
 * 用户再也无法判断「少的那几篇到底是被扫描丢了还是被索引丢了」。
 *
 * # 为什么没有「一键修复」
 *
 * 跳过的东西大多不可自动修：权限要用户自己改，GBK 文件名要用户自己改名，
 * 管道和设备要不要留是用户的决定。一个「一键修复」按钮只能做一件事——删文件。
 * 所以这里只有清单与说明，一个动作按钮都没有（§5 红线 2）。
 */
export function SkipPanel({
  panelRef,
  scanSkipped,
  scanEntries,
  scanTruncated,
  indexStats,
  error,
  onClose,
}: SkipPanelProps) {
  useLayoutEffect(() => {
    panelRef.current?.focus()
  }, [panelRef])

  const indexState = indexStats?.indexState
  const building = indexState?.state === 'building'
  const indexTotal = indexStats?.total ?? 0
  const indexEntries = indexStats?.entries ?? []
  /**
   * 只进总数、不进清单的那几条（`Internal`）。
   *
   * 必须单独说一句，否则「总数 3、清单 2 条、又没说被截断」看起来像渲染丢了
   * 一行。`Internal` 是用户修不了的那一类，列出来只会让他以为能修。
   */
  const unlisted = scanTruncated ? 0 : Math.max(0, scanSkipped - scanEntries.length)

  /** 索引侧也一样：`Unknown` 之外理论上不该有，但总数与清单必须对得上。 */
  const indexUnlisted = indexStats?.truncated === true
    ? 0
    : Math.max(0, indexTotal - indexEntries.length)

  /**
   * 批次结束时把焦点收回面板。
   *
   * 索引建完之后「正在建立」那一段会消失——React 卸载节点、焦点掉到
   * `<body>`，而 Esc 监听挂在这个容器上：焦点一出去，面板就关不掉了。
   */
  useEffect(() => {
    if (!building) panelRef.current?.focus()
  }, [building, panelRef])

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault()
      onClose()
      return
    }
    if (e.key !== 'Tab') return

    // 首尾相接，理由与回收站 / 待转存面板一致：焦点跑出去之后 Esc 就收不到了。
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
        className="trash-panel skip-panel"
        role="dialog"
        aria-modal="true"
        aria-label="跳过的条目与笔记"
        tabIndex={-1}
        onKeyDown={handleKeyDown}
      >
        <header className="trash__head">
          <h2 className="trash__title">跳过</h2>
          <button type="button" className="btn btn--small" onClick={onClose}>
            关闭
          </button>
        </header>

        {error !== null && (
          <p className="trash__error" role="alert">
            {error}
          </p>
        )}

        {/*
          两栏。两个数**分别显示、分别计数**——不合并、不求和、不取最大。
          合并之后「侧栏说 3、搜索说 1」就重新变成一句无法解释的话。
        */}
        <section className="skip__section" aria-label="扫描时跳过">
          <h3 className="skip__heading">
            扫描时跳过
            <span className="skip__count">{scanSkipped} 个条目</span>
          </h3>
          <p className="skip__lead">
            这些是磁盘上的<strong>目录项</strong>：没有权限的目录、名字不是
            UTF-8 的文件、管道与设备文件、软链。它们没进左边的树。
          </p>

          {scanSkipped === 0 ? (
            <p className="trash__empty">扫描时没有跳过任何条目。</p>
          ) : (
            <ul className="trash__list" aria-label="扫描时跳过的条目">
              {scanEntries.map((entry, index) => {
                const label = scanSkipPath(entry)
                const copy = describeScanSkip(entry.reason)
                return (
                  <li key={`${entry.path ?? 'none'}-${index}`} className="skip__item">
                    <div className="skip__row">
                      <span className="encoding__path" title={label.text}>
                        {label.text}
                      </span>
                      {label.lossy && (
                        // 红线：lossy 路径必须带可见标记。U+FFFD 冒充真名字，
                        // 用户照着它去改名会找不到文件。
                        <span className="skip__flag">名字含无法显示的字符</span>
                      )}
                    </div>
                    <p className="skip__what">{copy.what}</p>
                    <p className="skip__fix">{copy.fix}</p>
                  </li>
                )
              })}
            </ul>
          )}

          {scanTruncated && (
            <p className="trash__notice" role="status">
              清单只列出前 {scanEntries.length} 条（共 {scanSkipped} 个条目）。
            </p>
          )}
          {unlisted > 0 && (
            <p className="trash__notice" role="status">
              另有 {unlisted} 个条目只计入总数：它们不是你能修的问题。
            </p>
          )}
        </section>

        <section className="skip__section" aria-label="索引时跳过">
          <h3 className="skip__heading">
            索引时跳过
            <span className="skip__count">{indexTotal} 篇笔记</span>
          </h3>
          <p className="skip__lead">
            这些是<strong>笔记</strong>：它们进了树，但没进搜索索引，所以搜不到。
            与上面那个数没有关系——一篇 3 MB 的笔记在上面是正常的树节点。
          </p>

          {/*
            索引还在建时把话说清楚。没有这一行，用户看到 0 会以为库里没有
            跳过的笔记——而真相是「还有 9900 篇没数到」。
          */}
          {building && indexState?.state === 'building' && (
            <p className="trash__notice" role="status">
              {buildingHint(indexState.done, indexState.total)}
            </p>
          )}

          {indexTotal === 0 ? (
            <p className="trash__empty">
              {indexStats === null
                ? '读取中…'
                : building
                  ? '索引还在建，这个数会长。'
                  : '索引里没有跳过的笔记。'}
            </p>
          ) : (
            <ul className="trash__list" aria-label="索引时跳过的笔记">
              {indexEntries.map((entry) => {
                const copy = describeIndexSkip(entry.reason)
                return (
                  <li key={entry.path} className="skip__item">
                    <div className="skip__row">
                      <span className="encoding__path" title={entry.path}>
                        {entry.path}
                      </span>
                    </div>
                    <p className="skip__what">{copy.what}</p>
                    <p className="skip__fix">{copy.fix}</p>
                  </li>
                )
              })}
            </ul>
          )}

          {indexStats?.truncated === true && (
            <p className="trash__notice" role="status">
              清单只列出前 {indexEntries.length} 篇（共 {indexTotal} 篇）。
            </p>
          )}
          {indexUnlisted > 0 && (
            <p className="trash__notice" role="status">
              另有 {indexUnlisted} 篇只计入总数：它们不是你能修的问题。
            </p>
          )}
        </section>

        <footer className="trash__footer">
          <span>
            这两个数不会相等，也不该相等：上面数的是<strong>条目</strong>
            （目录、软链、设备文件），下面数的是<strong>笔记</strong>。这里没有
            「一键修复」——跳过的东西大多要你自己改。
          </span>
          <span className="trash__keys">Esc 关闭</span>
        </footer>
      </div>
    </div>
  )
}
