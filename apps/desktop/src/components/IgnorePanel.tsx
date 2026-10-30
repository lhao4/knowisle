import { useLayoutEffect, type KeyboardEvent, type MouseEvent, type RefObject } from 'react'

import type { IgnoreRules } from '../api'
import { configErrorLine, hasUserRules, splitBuiltin } from '../lib/ignore'

interface IgnorePanelProps {
  panelRef: RefObject<HTMLDivElement | null>
  /** 当前库**生效**的规则（来自最近一次 `vault_scan`）。 */
  rules: IgnoreRules
  /** 正在重扫。按钮据此禁用并改文案。 */
  rescanning: boolean
  onRescan: () => void
  onClose: () => void
}

/**
 * 「当前忽略规则」——一个**只读**面板。
 *
 * # 它回答的是哪一个问题
 *
 * 「我明明有一个 `dist` 文件夹，侧栏里为什么没有？」这是 B-05 的原话。
 * 在它之前，这个问题的答案是「什么都没有」：目录被剪枝剪掉了，连 `skipped`
 * 都不计——因为扫描器根本没访问过它。用户唯一能做的是一次次改名试错。
 *
 * # 为什么它不并进跳过面板
 *
 * 跳过是「我试了，没成」（无权限、非 UTF-8 文件名、FIFO），出口是改权限 /
 * 改名 / 挪走；忽略是「我按规则没看」，出口是改配置 + 重扫。两种缺席，
 * 两个出口。并进一个面板之后，用户会对着一条正常的忽略规则去找「怎么修」，
 * 而它根本不需要修——它需要的是**知道**（ADR-013 / ADR-014）。
 *
 * # 为什么是只读
 *
 * 写配置需要「保存 + 重扫 + 出错时回滚」，而配置文件是用户自己的文件：
 * 应用替他改，等于把「用户输入」变成「应用状态」。这里只显示**去哪改**、
 * **改完要做什么**，以及**改错了是什么样**。
 */
export function IgnorePanel({
  panelRef,
  rules,
  rescanning,
  onRescan,
  onClose,
}: IgnorePanelProps) {
  useLayoutEffect(() => {
    panelRef.current?.focus()
  }, [panelRef])

  const builtin = splitBuiltin(rules)
  const disabled = builtin.filter((rule) => rule.disabled)
  const error = rules.configError

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault()
      onClose()
      return
    }
    if (e.key !== 'Tab') return

    // 首尾相接：焦点跑出去之后 Esc 就收不到了（与回收站 / 跳过面板同一条）。
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
        className="trash-panel ignore-panel"
        role="dialog"
        aria-modal="true"
        aria-label="当前忽略规则"
        tabIndex={-1}
        onKeyDown={handleKeyDown}
      >
        <header className="trash__head">
          <h2 className="trash__title">忽略规则</h2>
          <button type="button" className="btn btn--small" onClick={onClose}>
            关闭
          </button>
        </header>

        {/*
          配置写坏时，这一行必须在**最上面**：用户是带着「我的目录不见了」
          来的，而此刻真正的答案是「你的配置没生效，规则退回了默认」。
          把它放在名单下面等于让他先读 15 条不相关的规则。
        */}
        {error !== null && (
          <p className="trash__error ignore__error" role="alert">
            <span className="ignore__error-line">{configErrorLine(error)}</span>
            <span className="ignore__error-path">{error.path}</span>
            <span className="ignore__error-note">
              这份配置没有被采用：下面的名单是<strong>内置默认</strong>，你写的规则一条都没生效。
            </span>
          </p>
        )}

        <section className="skip__section" aria-label="配置文件">
          <h3 className="skip__heading">配置文件</h3>
          <p className="skip__lead">
            忽略规则写在库里，跟着库走。没有这个文件就是只用内置名单——那是绝大多数库的
            正常状态，不是错误。
          </p>
          <p className="ignore__path" title={rules.configPath ?? ''}>
            {rules.configPath ?? '（这次扫描没有读配置文件：只看内置名单）'}
          </p>
          <pre className="ignore__sample">{`[ignore]
extra  = ["我的笔记"]   # 这些名字当目录/文件处理，不进树
negate = ["logseq"]     # 停用一条内置规则（logseq 下的笔记回来了）`}</pre>
          <p className="ignore__hint">
            名字按<strong>目录名 / 文件名</strong>匹配（不分大小写），不按路径：
            写 <code>dist</code> 会忽略<strong>所有</strong>叫 dist 的目录。
            <code>.git</code> 与 <code>.knowisle</code> 永远不可停用。
          </p>
        </section>

        {/*
          「改完要重扫」是这个面板存在的前提之一：应用没有 watcher（ADR-011
          已否），改完配置不重扫就真的什么都没发生。不写这句，用户会以为
          自己改错了。
        */}
        <section className="skip__section" aria-label="生效方式">
          <h3 className="skip__heading">改完要重新扫描</h3>
          <p className="skip__lead">
            应用不监听文件变化（ADR-011）。编辑并保存配置文件之后，点下面的按钮，
            或者重新打开库。
          </p>
          <button
            type="button"
            className="btn btn--small ignore__rescan"
            onClick={onRescan}
            disabled={rescanning}
          >
            {rescanning ? '扫描中…' : '重新扫描'}
          </button>
        </section>

        <section className="skip__section" aria-label="内置忽略名单">
          <h3 className="skip__heading">
            内置名单
            <span className="skip__count">{builtin.length} 条</span>
            {disabled.length > 0 && (
              <span className="skip__count">{disabled.length} 条已停用</span>
            )}
          </h3>
          <p className="skip__lead">
            这些名字默认不进树：其他工具的元数据目录、构建产物与依赖、以及本应用自己的
            <code>.knowisle</code>。误伤就是这样发生的——一个真叫 <code>dist</code> 的
            笔记目录也会被剪掉。
          </p>
          <ul className="ignore__rules" aria-label="内置规则">
            {builtin.map((rule) => (
              <li
                key={rule.name}
                className={`ignore__rule${rule.disabled ? ' is-disabled' : ''}`}
              >
                <code>{rule.name}</code>
                {rule.disabled && <span className="ignore__flag">已停用</span>}
              </li>
            ))}
          </ul>
        </section>

        <section className="skip__section" aria-label="用户规则">
          <h3 className="skip__heading">
            你写的规则
            <span className="skip__count">{rules.extra.length + rules.negate.length} 条</span>
          </h3>
          {hasUserRules(rules) ? (
            <ul className="ignore__rules" aria-label="用户规则列表">
              {rules.extra.map((name) => (
                <li key={`extra-${name}`} className="ignore__rule">
                  <code>{name}</code>
                  <span className="ignore__flag ignore__flag--extra">额外忽略</span>
                </li>
              ))}
              {rules.negate.map((name) => (
                <li key={`negate-${name}`} className="ignore__rule">
                  <code>{name}</code>
                  <span className="ignore__flag ignore__flag--negate">已停用</span>
                </li>
              ))}
            </ul>
          ) : (
            <p className="trash__empty">还没有写过规则，用的是上面的内置名单。</p>
          )}
        </section>

        <footer className="trash__footer">
          <span>
            忽略不是跳过：跳过是「我试了，没成」（权限、编码、管道），这里是
            <strong>「我按规则没看」</strong>。被忽略的目录不会出现在「跳过 N」里，
            也不会被计数——扫描器根本没下去过。
          </span>
          <span className="trash__keys">Esc 关闭</span>
        </footer>
      </div>
    </div>
  )
}
