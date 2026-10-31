import { useLayoutEffect, type KeyboardEvent, type MouseEvent, type RefObject } from 'react'

import type { ScanResult, VaultInfo } from '../api'
import { formatBytes } from '../lib/format'
import { truncationNotice } from '../lib/truncation'
import { describeIndexState, formatScanMs } from '../lib/vault-info'

interface VaultInfoPanelProps {
  panelRef: RefObject<HTMLDivElement | null>
  /** 最近一次扫描结果。面板上的「规模」全部来自它，不再发一次 IPC。 */
  vault: ScanResult
  /** 库里有多少篇不是 UTF-8（待转存）。`0` = 没有这一行。 */
  encodingPending: number
  /** 只读快照。`null` = 还没取到（首次打开的那一瞬）。 */
  info: VaultInfo | null
  error: string | null
  busy: boolean
  onRefresh: () => void
  onClose: () => void
  onOpenSkip: () => void
  onOpenEncoding: () => void
  onOpenIgnore: () => void
}

/**
 * 库信息：**只读诊断面**，一个写操作都没有。
 *
 * # 它回答的问题
 *
 * 「这个库现在是什么状态」——多大、索引好了没、回收站里还压着多少、有没有
 * 东西没进树。这四件事此前散在四个地方（侧栏统计、搜索 footer、跳过入口、
 * 回收站徽标），而「索引库占了多少磁盘」根本无处可查。
 *
 * # 为什么一个按钮都不做
 *
 * 同一间屋子里的四个面板已经各自带着动作（恢复、清空、转存、翻页）。再往
 * 这里塞「重建索引」「打开目录」，就得开始处理「重建时正在编辑」「清空时
 * 二次确认」这些并发路径——那是另一刀的事，而且大概率属于设置页（B-29 的
 * 清理策略就卡在那上面）。这一刀只负责**看见**。
 *
 * # 每个数字都带口径
 *
 * 「索引库 1.2 MB」如果不说是哪几个文件，「回收站 200 MB」如果不说是文件还是
 * 含清单，面板就会变成新的误解来源。文案里的每一句口径都能在
 * `ki_core::info` 的文档里找到出处。
 */
export function VaultInfoPanel({
  panelRef,
  vault,
  encodingPending,
  info,
  error,
  busy,
  onRefresh,
  onClose,
  onOpenSkip,
  onOpenEncoding,
  onOpenIgnore,
}: VaultInfoPanelProps) {
  useLayoutEffect(() => {
    panelRef.current?.focus()
  }, [panelRef])

  const indexCopy = info === null ? null : describeIndexState(info.indexState)
  const { builtin, extra, negate } = vault.ignoreRules
  const hasConfigError = vault.ignoreRules.configError !== null

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault()
      onClose()
      return
    }
    if (e.key !== 'Tab') return

    // 首尾相接，理由与回收站 / 跳过 / 待转存一致：焦点跑出去之后 Esc 就收不到了。
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
        className="trash-panel info-panel"
        role="dialog"
        aria-modal="true"
        aria-label="库信息"
        tabIndex={-1}
        onKeyDown={handleKeyDown}
      >
        <header className="trash__head">
          <h2 className="trash__title">库信息</h2>
          <div className="info__actions">
            <button type="button" className="btn btn--small" onClick={onRefresh} disabled={busy}>
              {busy ? '读取中…' : '刷新'}
            </button>
            <button type="button" className="btn btn--small" onClick={onClose}>
              关闭
            </button>
          </div>
        </header>

        {error !== null && (
          <p className="trash__error" role="alert">
            {error}
          </p>
        )}

        <section className="info__section" aria-label="这个库">
          <h3 className="skip__heading">这个库</h3>
          {/*
            路径必须能整段选中。没有「复制」按钮是刻意的：WebView 的剪贴板
            权限在两个平台上的行为不一致，而 `user-select: all` 让单击一次就
            选中整段，Ctrl+C 就够了。
          */}
          <p className="info__path" title={vault.root}>
            {vault.root}
          </p>
          <p className="info__hint">点一下路径可以整段选中，再 Ctrl+C 复制。</p>
        </section>

        <section className="info__section" aria-label="规模">
          <h3 className="skip__heading">规模</h3>
          <dl className="info__grid">
            <div className="info__cell">
              <dt>笔记</dt>
              <dd>
                {vault.totalFiles} 篇
                {/* 截断时这个数是下界，必须当场说出来（ADR-015 的同一条公理）。 */}
                {vault.truncated && <span className="info__flag">下界</span>}
              </dd>
            </div>
            <div className="info__cell">
              <dt>目录</dt>
              <dd>{vault.totalDirs} 个</dd>
            </div>
            <div className="info__cell">
              <dt>上次扫描</dt>
              <dd>{formatScanMs(vault.elapsedMs)}</dd>
            </div>
          </dl>
          {vault.truncated && (
            <p className="trash__notice" role="status">
              {truncationNotice(vault.limitEntries)}
            </p>
          )}
        </section>

        <section className="info__section" aria-label="占用">
          <h3 className="skip__heading">这个应用占了多少</h3>
          <dl className="info__grid">
            <div className="info__cell">
              <dt>索引库</dt>
              <dd>{info === null ? '读取中…' : formatBytes(info.indexBytes)}</dd>
            </div>
            <div className="info__cell">
              <dt>回收站</dt>
              <dd>
                {info === null
                  ? '读取中…'
                  : `${formatBytes(info.trashBytes)}（${info.trashEntries} 个文件）`}
              </dd>
            </div>
          </dl>
          <p className="info__hint">
            索引库含 <code>-wal</code> / <code>-shm</code> 三个文件；回收站只算文件，不含记录清单。
            <strong>笔记本身的体积不在内</strong>——那是你的文件，不是这个应用占的空间。
          </p>
        </section>

        <section className="info__section" aria-label="索引">
          <h3 className="skip__heading">索引</h3>
          {indexCopy === null ? (
            <p className="trash__empty">读取中…</p>
          ) : (
            <>
              <p className="info__row">
                <span className="info__label">{indexCopy.label}</span>
              </p>
              <p className="info__hint">{indexCopy.detail}</p>
            </>
          )}
        </section>

        <section className="info__section" aria-label="没进树的东西">
          <h3 className="skip__heading">没进树的东西</h3>
          <p className="info__lead">
            三种缺席、三个出口：<strong>跳过</strong>（我试了没成）、
            <strong>待转存</strong>（能读，但不是 UTF-8）、
            <strong>忽略</strong>（我按规则没看）。它们的数字互不相加。
          </p>

          <div className="info__exits">
            <button
              type="button"
              className="btn btn--small info__exit"
              onClick={onOpenSkip}
              disabled={vault.skipped === 0}
              title={
                vault.skipped === 0
                  ? '这次扫描没有跳过任何条目'
                  : '查看被跳过的条目与笔记'
              }
            >
              {vault.skipped === 0 ? '没有条目被跳过' : `跳过 ${vault.skipped} 个条目`}
            </button>
            <button
              type="button"
              className="btn btn--small info__exit"
              onClick={onOpenEncoding}
              disabled={encodingPending === 0}
              title={
                encodingPending === 0
                  ? '库里所有笔记都是 UTF-8'
                  : '查看不是 UTF-8 的笔记并批量转存'
              }
            >
              {encodingPending === 0 ? '全部是 UTF-8' : `待转存 ${encodingPending} 篇`}
            </button>
            <button
              type="button"
              className="btn btn--small info__exit"
              onClick={onOpenIgnore}
              title="查看当前生效的忽略规则"
            >
              忽略规则：内置 {builtin.length} 条
              {extra.length > 0 && ` · 你加了 ${extra.length}`}
              {negate.length > 0 && ` · 停用 ${negate.length}`}
            </button>
          </div>

          {hasConfigError && (
            <p className="trash__notice" role="status">
              忽略配置读不出来，已退回内置名单。点上面的「忽略规则」看具体原因。
            </p>
          )}
        </section>

        <footer className="trash__footer">
          <span>
            这里<strong>没有任何写操作</strong>：只回答「现在是什么状态」。
            要改什么，去对应的面板。
          </span>
          <span className="trash__keys">Esc 关闭</span>
        </footer>
      </div>
    </div>
  )
}
