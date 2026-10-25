import { useLayoutEffect, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode } from 'react'

interface ConfirmDialogProps {
  /** 标题。一句疑问句，例如「删除这篇笔记？」。 */
  title: string
  /** 确认按钮文案。 */
  confirmLabel: string
  /**
   * 正文。用节点而不是字符串：危险操作的说明通常要分两层——
   * 一句"会发生什么"，一句"代价是什么"（后者更小、更淡，但不能省）。
   */
  children: ReactNode
  /**
   * 提交。返回 `null` 表示成功（浮层自行关闭），返回字符串表示失败原因。
   *
   * 与 `NamePrompt` 同一条契约：唯一的失败来源是 IPC，而错误已经被
   * `describeError` 翻成了一句人话。用返回值表达，调用方就不需要 try/catch，
   * 也就不会有人忘了 catch 而让浮层卡在「处理中…」。
   */
  onSubmit: () => Promise<string | null>
  onClose: () => void
}

/**
 * 危险操作的二次确认。
 *
 * # 只服务破坏性动作
 *
 * 确认按钮恒为 `--ki-danger` 红色，没有 `variant` 参数：本组件存在的唯一理由
 * 是"这一步做错了要丢东西"。将来若出现非破坏性的确认（"要保存吗？"），
 * 那是另一个组件——把两者合并成一个带 tone 参数的通用弹窗，等于让"红色"
 * 变成一次可以忘记传的选项。
 *
 * # 默认焦点在「取消」
 *
 * 这是本组件最重要的一个决定。焦点若落在确认按钮上，用户按回车（"我就想
 * 关掉这个框"）就会执行删除——**回车误触的代价远大于多点一次**。
 * 所以 `useLayoutEffect` 里显式聚焦取消按钮，而且这里**不监听 Enter**：
 * 让焦点所在的原生 button 自己处理（回车 = 激活当前聚焦的那个 = 取消）。
 *
 * # 关闭方式
 *
 * Esc、点浮层外、点取消——三条都走 `onClose`，都不产生副作用。
 */
export function ConfirmDialog({
  title,
  confirmLabel,
  children,
  onSubmit,
  onClose,
}: ConfirmDialogProps) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const cancelRef = useRef<HTMLButtonElement>(null)

  // 布局阶段聚焦：等到 `useEffect` 时，用户可能已经按下过一次回车了。
  useLayoutEffect(() => {
    cancelRef.current?.focus()
  }, [])

  const submit = async () => {
    if (busy) return
    setBusy(true)
    setError(null)

    let failure: string | null
    try {
      failure = await onSubmit()
    } catch (err) {
      // 兜底：调用方理应把异常翻成文案返回，漏了也不能让浮层卡住。
      failure = err instanceof Error ? err.message : String(err)
    }

    if (failure === null) {
      onClose()
      return
    }
    setBusy(false)
    setError(failure)
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'Escape') return
    e.preventDefault()
    if (!busy) onClose()
  }

  /** 点浮层外的空白处取消。判据与其它浮层一致：`target === currentTarget`。 */
  const handleBackdrop = (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget && !busy) onClose()
  }

  return (
    <div className="confirm-backdrop" onMouseDown={handleBackdrop}>
      <div
        className="confirm"
        role="alertdialog"
        aria-modal="true"
        aria-label={title}
        onKeyDown={handleKeyDown}
      >
        <h2 className="confirm__title">{title}</h2>
        <div className="confirm__body">{children}</div>

        {error !== null && (
          <p className="confirm__error" role="alert">
            {error}
          </p>
        )}

        <div className="confirm__actions">
          {/* 顺序：取消在前、危险按钮在后。视觉与 Tab 顺序一致，
              而默认焦点又落在第一个（取消）上——两者不冲突。 */}
          <button
            ref={cancelRef}
            type="button"
            className="btn btn--small"
            onClick={onClose}
            disabled={busy}
          >
            取消
          </button>
          <button
            type="button"
            className="btn btn--small btn--danger"
            onClick={() => void submit()}
            disabled={busy}
          >
            {busy ? '处理中…' : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  )
}
