import {
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type MouseEvent,
} from 'react'

import { QUICK_LIMIT, searchQuick, type QuickItem } from '../lib/quick-open'

interface QuickOpenProps {
  /** 扫描完成时扁平化好的全量候选。引用在两次扫描之间不变。 */
  items: QuickItem[]
  onOpen: (path: string) => void
  onClose: () => void
}

/** `aria-activedescendant` 指向的 id。序号即身份，列表每次查询都重建。 */
function optionId(index: number): string {
  return `ki-quick-opt-${index}`
}

/**
 * `Cmd/Ctrl+P` 浮层。
 *
 * # 为什么不用原生 `<dialog>`
 *
 * `showModal()` 自带焦点陷阱与 Esc→`cancel` 行为，但三套 WebView 的实现
 * 并不一致（尤其 Esc 在 `cancel` 与 `keydown` 上的先后、以及关闭后焦点
 * 落到哪）。它还会和 CM6 的 keymap 抢同一批按键——CM6 在 `keydown` 里
 * 处理 `Escape`，而 `<dialog>` 在 `cancel` 里处理，两者谁先谁后没有契约。
 * 用一个 fixed 的 div 就没有这些不确定性：焦点归谁、Esc 归谁，全由下面的
 * 十几行决定。
 *
 * # 列表项永远不获得焦点
 *
 * `li` 上没有 `tabIndex`，点击时 `onMouseDown` 阻止默认行为，于是焦点
 * 自始至终只在 `input` 上。当前项用 `aria-activedescendant` 指向——
 * 这样就不存在「焦点跑到列表项上、方向键被列表吃掉、Esc 收不到」这一整类
 * 问题，也不需要手写焦点陷阱。
 *
 * # 每次打开都是新的
 *
 * 组件由上层条件渲染，关闭即卸载。查询与选中项自然归零，不需要
 * 「下次打开要清空」这种容易漏掉的复位逻辑。
 */
export function QuickOpen({ items, onOpen, onClose }: QuickOpenProps) {
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  const inputRef = useRef<HTMLInputElement>(null)
  const listRef = useRef<HTMLUListElement>(null)
  /**
   * 是否正在 IME 组字。
   *
   * 不能只看 `KeyboardEvent.isComposing`：WebKit 会在 `compositionend`
   * **之前**派发确认组字的那个 `keydown`，此时 `isComposing` 已经是 false。
   * 中文用户按 Enter 上屏的第一下就会踩到，表现为「刚打完字，笔记被打开了」。
   * 所以自己记一份，与 `isComposing`、`keyCode === 229` 三者取并集。
   */
  const composingRef = useRef(false)

  const results = useMemo(() => searchQuick(items, query), [items, query])
  const index = results.length === 0 ? -1 : Math.min(active, results.length - 1)

  useLayoutEffect(() => {
    inputRef.current?.focus()
  }, [])

  // 选中项滚进可视区。`nearest` 是关键：`start` 会让列表每动一格就跳一下。
  useLayoutEffect(() => {
    if (index < 0) return
    listRef.current?.children[index]?.scrollIntoView({ block: 'nearest' })
  }, [index])

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    // 组字期间一个键都不处理：Enter 是上屏，Esc 是取消候选，方向键是选词。
    if (composingRef.current || e.nativeEvent.isComposing || e.nativeEvent.keyCode === 229) {
      return
    }

    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault()
        if (results.length > 0) {
          setActive(Math.min(Math.min(active, results.length - 1) + 1, results.length - 1))
        }
        break
      case 'ArrowUp':
        e.preventDefault()
        setActive(Math.max(Math.min(active, results.length - 1) - 1, 0))
        break
      case 'Enter':
        e.preventDefault()
        if (index >= 0) onOpen(results[index].path)
        break
      case 'Escape':
        e.preventDefault()
        onClose()
        break
      case 'Tab':
        // 列表里没有可聚焦元素，Tab 只会把焦点甩出浮层，之后 Esc 就收不到了。
        // 浮层里也没有第二个该去的地方，直接吃掉。
        e.preventDefault()
        break
      default:
        break
    }
  }

  /**
   * 点浮层外的空白处关闭。
   *
   * 判据是 `target === currentTarget`：点面板内部时 target 是面板或它的子节点，
   * 事件冒泡到这里也不会误关。用 `mousedown` 而不是 `click`——按下就关，
   * 与「点哪都先关掉再说」的直觉一致。
   */
  const handleBackdrop = (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget) onClose()
  }

  const summary =
    query.length === 0
      ? '没有打开历史，按侧栏顺序'
      : results.length >= QUICK_LIMIT
        ? `前 ${QUICK_LIMIT} 个结果`
        : `${results.length} 个结果`

  return (
    <div className="quick-open-backdrop" onMouseDown={handleBackdrop}>
      <div
        className="quick-open"
        role="dialog"
        aria-modal="true"
        aria-label="快速打开笔记"
        onKeyDown={handleKeyDown}
      >
        <input
          ref={inputRef}
          className="quick-open__input"
          type="text"
          role="combobox"
          aria-expanded="true"
          aria-controls="ki-quick-list"
          aria-autocomplete="list"
          aria-activedescendant={index >= 0 ? optionId(index) : undefined}
          placeholder="输入文件名或路径…"
          value={query}
          spellCheck={false}
          autoComplete="off"
          onChange={(e) => {
            setQuery(e.target.value)
            // 查询变了，旧的下标指向的是另一批结果，必须回到第一条。
            setActive(0)
          }}
          onCompositionStart={() => {
            composingRef.current = true
          }}
          onCompositionEnd={() => {
            composingRef.current = false
          }}
        />

        {results.length === 0 ? (
          <p className="quick-open__empty">没有匹配的笔记</p>
        ) : (
          <ul
            className="quick-open__list"
            id="ki-quick-list"
            role="listbox"
            aria-label="匹配结果"
            ref={listRef}
          >
            {results.map((item, i) => (
              <li
                key={item.path}
                id={optionId(i)}
                role="option"
                aria-selected={i === index}
                data-path={item.path}
                className={`quick-open__item${i === index ? ' is-active' : ''}`}
                // 点击不抢焦点：焦点一旦离开 input，Esc 与方向键就收不到了。
                onMouseDown={(e) => e.preventDefault()}
                onMouseEnter={() => {
                  if (i !== index) setActive(i)
                }}
                onClick={() => onOpen(item.path)}
              >
                <span className="quick-open__name">{item.name}</span>
                {item.dir !== '' && <span className="quick-open__dir">{item.dir}</span>}
              </li>
            ))}
          </ul>
        )}

        <div className="quick-open__footer">
          <span className="quick-open__summary">{summary}</span>
          <span className="quick-open__keys">↑↓ 选择 · Enter 打开 · Esc 关闭</span>
        </div>
      </div>
    </div>
  )
}
