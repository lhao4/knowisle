import {
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type MouseEvent,
} from 'react'

interface NamePromptProps {
  /** 标题，例如「新建笔记」「重命名」。 */
  title: string
  /** 初始值：新建为空串，重命名是当前文件名（含扩展名）。 */
  initial: string
  /** 确认按钮文案。 */
  confirmLabel: string
  /**
   * 提交。返回 `null` 表示成功（浮层自行关闭），返回字符串表示失败原因
   * （浮层留在原地，把理由显示在输入框下面）。
   *
   * 为什么是"返回错误"而不是"抛异常"：这里唯一的失败来源是 IPC，
   * 而 IPC 的错误已经被 `describeError` 翻成了一句人话。用返回值表达，
   * 调用方就不需要写 try/catch，也就不会有人忘了 catch 而让浮层卡在
   * 「确定」的 loading 态。
   */
  onSubmit: (name: string) => Promise<string | null>
  onClose: () => void
}

/**
 * 输入文件名的浮层。新建与重命名共用。
 *
 * # 为什么不用 `window.prompt`
 *
 * WKWebView（macOS）根本不实现 `prompt()`，调用会返回 `null` 且没有任何提示。
 * 用同一份代码在三平台上得到三种行为，是这类"原生小控件"最典型的坑。
 *
 * # 前端的校验只是即时反馈
 *
 * {@link localHint} 只覆盖**Rust 一定也会拒绝**的几种形态（空、斜杠、尾部点）。
 * 它是为了让用户在按下确定之前就知道哪里不对，**不是**闸门：
 * 真正的判定在 `ki_vault::naming`，将来 `isle` CLI 走的也是那里。
 * 所以这里宁可少判几条，也不要判出 Rust 放行的名字——那会让 GUI 比 CLI 更严，
 * 而用户没有任何办法绕开。
 */
export function NamePrompt({ title, initial, confirmLabel, onSubmit, onClose }: NamePromptProps) {
  const [value, setValue] = useState(initial)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const inputRef = useRef<HTMLInputElement>(null)
  /** IME 组字中。理由与快速打开逐字相同：WebKit 会在 compositionend 之前派发 Enter。 */
  const composingRef = useRef(false)

  useLayoutEffect(() => {
    const input = inputRef.current
    if (input === null) return
    input.focus()
    // 只选中主名，留下扩展名：改名时最常见的操作是"换个主名"，
    // 全选会把 `.md` 一起删掉，用户还得自己补回来（补是会自动补，但看着像丢了）。
    const dot = initial.lastIndexOf('.')
    input.setSelectionRange(0, dot > 0 ? dot : initial.length)
  }, [initial])

  const hint = localHint(value)
  const blocked = busy || hint !== null

  const submit = async () => {
    if (blocked) return
    setBusy(true)
    setError(null)

    let failure: string | null
    try {
      failure = await onSubmit(value)
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
    if (composingRef.current || e.nativeEvent.isComposing || e.nativeEvent.keyCode === 229) {
      return
    }
    if (e.key === 'Enter') {
      e.preventDefault()
      void submit()
      return
    }
    if (e.key === 'Escape') {
      e.preventDefault()
      if (!busy) onClose()
    }
  }

  /** 点浮层外的空白处取消。判据与其它浮层一致：`target === currentTarget`。 */
  const handleBackdrop = (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget && !busy) onClose()
  }

  return (
    <div className="prompt-backdrop" onMouseDown={handleBackdrop}>
      <div
        className="prompt"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onKeyDown={handleKeyDown}
      >
        <h2 className="prompt__title">{title}</h2>

        <input
          ref={inputRef}
          className="prompt__input"
          type="text"
          value={value}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={hint !== null || error !== null}
          onChange={(e) => {
            setValue(e.target.value)
            // 一改输入就把上一次的服务端错误收掉：它说的是旧名字。
            setError(null)
          }}
          onCompositionStart={() => {
            composingRef.current = true
          }}
          onCompositionEnd={() => {
            composingRef.current = false
          }}
        />

        <p className="prompt__hint">没写后缀会自动补 .md；不能包含 \ / : * ? " &lt; &gt; |</p>
        {hint !== null && <p className="prompt__error">{hint}</p>}
        {error !== null && (
          <p className="prompt__error" role="alert">
            {error}
          </p>
        )}

        <div className="prompt__actions">
          <button type="button" className="btn btn--small" onClick={onClose} disabled={busy}>
            取消
          </button>
          <button
            type="button"
            className="btn btn--small btn--primary"
            onClick={() => void submit()}
            disabled={blocked}
          >
            {busy ? '处理中…' : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  )
}

/**
 * 只判"Rust 一定也会拒绝"的几种形态。
 *
 * 每一条都能在 `ki_vault::naming` 里找到对应规则；这里**不新增**任何规则。
 * 少判一条的后果只是"按下确定才报错"，多判一条的后果是"GUI 拦住了 CLI 允许的
 * 名字"——后者更糟，所以这张表刻意短。
 */
function localHint(value: string): string | null {
  if (value.trim() === '') return '名字不能为空'
  if (value.trim() !== value) return '名字首尾不能有空白'
  if (value.includes('/') || value.includes('\\')) return '名字里不能有斜杠'
  if (value.endsWith('.')) return '名字不能以点结尾'
  return null
}
