import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type MouseEvent,
} from 'react'

import { describeError, vaultSearch, type SearchHit, type SearchResult } from '../api'

interface SearchOverlayProps {
  /** Vault 根目录（`ScanResult.root`，已规范化）。 */
  root: string
  /** 当前笔记路径；没有打开笔记时为 `null`。Rust 每次搜索都会重读它一次。 */
  activePath: string | null
  /**
   * 第一次查询之前要做的事：把编辑器里的内容推给 Rust 并落盘。
   *
   * 为什么由上层提供而不是在这里调 `noteFlush`：冲刷要先 `getContent()`
   * 拿到合并窗口里还没发出去的字符，那是 `NoteEditorHandle` 的职责。
   * 失败不阻断搜索（调用方自己展示冲突横幅），这里只等它结束。
   */
  onPrepare: () => Promise<void>
  onOpen: (hit: SearchHit) => void
  onClose: () => void
  /**
   * 点 footer 上那个「跳过 M」。
   *
   * 它是**索引侧**的数（`status='skipped'` 的笔记），与侧栏那个扫描侧的条目数
   * 是两件事。两个数分开显示、分开计数，都能点开看到各自的清单——但打开的是
   * 同一个面板，因为面板里两栏都在（ADR-013）。
   */
  onOpenSkipped: () => void
}

/** `aria-activedescendant` 指向的 id。序号即身份，列表每次查询都重建。 */
function optionId(index: number): string {
  return `ki-search-opt-${index}`
}

/**
 * `Cmd/Ctrl+Shift+F` 全文搜索浮层。
 *
 * # 外壳沿用快速打开，但**不抽公共组件**
 *
 * fixed div + input + listbox + `aria-activedescendant` + IME 三防线 + Esc 归还焦点，
 * 这六条已经被快速打开验证过（尤其是不用原生 `<dialog>`、列表项永不获得焦点）。
 * 但两个浮层的数据源、状态机、渲染行完全不同：一个是内存里的 1000 个字符串，
 * 一个是异步 IPC + 两套偏移量 + 分组排序。现在抽出来的"公共浮层"只会有两种命运：
 * 要么被 props 撑成一个配置对象，要么在下一次需求里被绕开。
 * 第三次使用（命令面板）才知道哪些是真共性——那时再抽。
 *
 * # 为什么没有防抖
 *
 * 每次击键发一次 IPC。1000 篇的库，一次子串扫描是毫秒级，加一次往返也远低于
 * 一帧；而防抖会把"按下去到看见结果"凭空拉长 100ms 以上，正好吃掉
 * 150ms 的预算。真正需要挡住的是**乱序**：晚到的旧响应会把新结果覆盖掉，
 * 所以每次请求带一个序号，只有最新那次允许写 state。
 *
 * # 为什么先等 `onPrepare`
 *
 * 搜索前的落盘是异步的。若不等它，用户在打开浮层的瞬间输入，第一次查询
 * 可能读到落盘前的磁盘——于是"刚打的字搜不到"，而这恰恰是决策 c 要消灭的
 * 那类 bug。`ready` 之前不查，输入框照常可以打字。
 */
export function SearchOverlay({
  root,
  activePath,
  onPrepare,
  onOpen,
  onClose,
  onOpenSkipped,
}: SearchOverlayProps) {
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  /**
   * 最近一次结果，**连同它对应的查询**一起存。
   *
   * 不这样做的话，查询从 "foo" 变成 "bar" 的那一帧里，`result` 还是 foo 的
   * 那批行——列表会先闪一下旧结果再换成新的。把 needle 存进来，渲染时
   * 对不上就当没有结果，不需要在 effect 里同步 `setResult(null)` 去清空。
   */
  const [answer, setAnswer] = useState<{ needle: string; data: SearchResult } | null>(null)
  const [failure, setFailure] = useState<{ needle: string; message: string } | null>(null)
  /** 搜索前的落盘是否结束。`false` 时一次查询都不发。 */
  const [ready, setReady] = useState(false)
  const inputRef = useRef<HTMLInputElement>(null)
  const listRef = useRef<HTMLUListElement>(null)
  /** IME 组字中。理由与快速打开逐字相同，见 `QuickOpen` 的注释。 */
  const composingRef = useRef(false)
  /** 最新一次请求的序号。晚到的旧响应直接丢弃。 */
  const seqRef = useRef(0)

  const needle = query.trim()
  const result = answer !== null && answer.needle === needle ? answer.data : null
  const error = failure !== null && failure.needle === needle ? failure.message : null
  const hits = result?.hits ?? []
  /**
   * 索引状态。**只跟着结果走**，不轮询。
   *
   * `vault_scan` 只等树，索引在后台建；这时搜到的结果可能不全。状态是
   * `SearchResult` 的一部分（不是另一次 IPC），所以它和这批命中来自同一次读——
   * 不会出现「状态说在建、结果已经全了」这种要前端自己判断的情况。
   */
  const indexing = result?.indexState ?? null
  const index = hits.length === 0 ? -1 : Math.min(active, hits.length - 1)
  /**
   * 「还在搜」是**推出来的**，不是一个 state。
   *
   * 它只有一种含义：这个 needle 还没有答案，也没有报错。写成 state 就要在
   * 每次查询开始时同步 `setPending(true)`——那是在 effect 里同步 setState，
   * 会白白多渲染一轮，而且容易和"查询变了但请求还没回来"脱节。
   */
  const busy = !ready || (needle !== '' && result === null && error === null)

  useLayoutEffect(() => {
    inputRef.current?.focus()
  }, [])

  useEffect(() => {
    let alive = true
    onPrepare()
      .catch(() => {
        // 失败不阻断搜索：冲突横幅由 App 的 saveState 驱动，这里没有话可说。
      })
      .finally(() => {
        if (alive) setReady(true)
      })
    return () => {
      alive = false
    }
  }, [onPrepare])

  useEffect(() => {
    if (!ready) return
    const needle = query.trim()
    // 空查询不建索引、不发 IPC：首次打开浮层不该付出全库读盘的代价。
    // 也不用清空结果——渲染时按 needle 对不上就自然看不见。
    if (needle === '') return

    const seq = ++seqRef.current
    let alive = true

    vaultSearch(root, needle, activePath)
      .then((next) => {
        if (!alive || seqRef.current !== seq) return
        setAnswer({ needle, data: next })
        setFailure(null)
        // 结果换了，旧下标指向的是另一批行。
        setActive(0)
      })
      .catch((err: unknown) => {
        if (!alive || seqRef.current !== seq) return
        setAnswer(null)
        setFailure({ needle, message: describeError(err) })
      })

    return () => {
      alive = false
    }
  }, [ready, query, root, activePath])

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
        if (hits.length > 0) {
          setActive(Math.min(Math.min(active, hits.length - 1) + 1, hits.length - 1))
        }
        break
      case 'ArrowUp':
        e.preventDefault()
        setActive(Math.max(Math.min(active, hits.length - 1) - 1, 0))
        break
      case 'Enter':
        e.preventDefault()
        if (index >= 0) onOpen(hits[index])
        break
      case 'Escape':
        e.preventDefault()
        onClose()
        break
      case 'Tab':
        // 列表里没有可聚焦元素，Tab 只会把焦点甩出浮层，之后 Esc 就收不到了。
        e.preventDefault()
        break
      default:
        break
    }
  }

  /** 点浮层外的空白处关闭。判据与快速打开一致：`target === currentTarget`。 */
  const handleBackdrop = (e: MouseEvent<HTMLDivElement>) => {
    if (e.target === e.currentTarget) onClose()
  }

  return (
    <div className="search-backdrop" onMouseDown={handleBackdrop}>
      <div
        className="search-panel"
        role="dialog"
        aria-modal="true"
        aria-label="全文搜索"
        onKeyDown={handleKeyDown}
      >
        <input
          ref={inputRef}
          className="search__input"
          type="text"
          role="combobox"
          aria-expanded="true"
          aria-controls="ki-search-list"
          aria-autocomplete="list"
          aria-activedescendant={index >= 0 ? optionId(index) : undefined}
          placeholder="搜索全文…"
          value={query}
          spellCheck={false}
          autoComplete="off"
          onChange={(e) => {
            setQuery(e.target.value)
            setActive(0)
          }}
          onCompositionStart={() => {
            composingRef.current = true
          }}
          onCompositionEnd={() => {
            composingRef.current = false
          }}
        />

        {indexing !== null && indexing.state === 'building' && (
          // 这一行是「后台索引」的可见性契约：没有它，搜不到刚扫描进来的文件
          // 就是一句无从解释的空白。放在列表**上方**，因为它修饰的是整批结果。
          <p className="search__indexing" role="status">
            索引正在建立（{indexing.done}/{indexing.total}），以下结果可能不全
          </p>
        )}
        {indexing !== null && indexing.state === 'failed' && (
          <p className="search__indexing is-failed" role="status">
            索引建立失败，以下结果可能不全：{indexing.message}
          </p>
        )}

        {error !== null ? (
          <p className="search__empty">{error}</p>
        ) : hits.length === 0 ? (
          <p className="search__empty">
            {needle === ''
              ? '输入关键词，匹配的是原文子串（不区分 ASCII 大小写）。'
              : busy
                ? '搜索中…'
                : indexing?.state === 'building'
                  ? '索引还在建立，暂时没有匹配的行'
                  : '没有匹配的行'}
          </p>
        ) : (
          <ul
            className="search__list"
            id="ki-search-list"
            role="listbox"
            aria-label="匹配的命中行"
            ref={listRef}
          >
            {hits.map((hit, i) => (
              <li
                key={`${hit.path}:${hit.line}`}
                id={optionId(i)}
                role="option"
                aria-selected={i === index}
                data-path={hit.path}
                data-line={hit.line}
                className={`search__item${i === index ? ' is-active' : ''}`}
                // 点击不抢焦点：焦点一旦离开 input，Esc 与方向键就收不到了。
                onMouseDown={(e) => e.preventDefault()}
                onMouseEnter={() => {
                  if (i !== index) setActive(i)
                }}
                onClick={() => onOpen(hit)}
              >
                <span className="search__head">
                  <span className="search__path">{hit.path}</span>
                  <span className="search__line">:{hit.line}</span>
                  {hit.nameHit && <span className="search__badge">文件名</span>}
                </span>
                <span className="search__snippet">
                  {hit.text.slice(0, hit.from)}
                  {/* 命中片段用 DOM 节点包住。绝不拼 innerHTML：正文来自磁盘，
                      一个 `<img onerror=...>` 就是一个执行入口。 */}
                  <mark className="search__mark">{hit.text.slice(hit.from, hit.to)}</mark>
                  {hit.text.slice(hit.to)}
                </span>
              </li>
            ))}
          </ul>
        )}

        <div className="search__footer">
          <span className="search__summary">{summaryOf(result, error, busy)}</span>
          <span className="search__foot-actions">
            {/*
              索引侧的「跳过 M」。**必须是按钮**：这个数此前是一个说不清的
              整数，用户查不了「是哪几篇」。点开之后是同一篇清单，但这里
              显示的数与侧栏那个（扫描侧的条目数）从头到尾没有合并过。
            */}
            {result !== null && result.skippedFiles > 0 && (
              <button
                type="button"
                className="search__skipped"
                onClick={onOpenSkipped}
                title="索引时跳过的笔记（认不出编码、超过 2MB、IO 失败），点开看是哪几篇"
              >
                跳过 {result.skippedFiles}
              </button>
            )}
            <span className="search__keys">↑↓ 选择 · Enter 跳转 · Esc 关闭</span>
          </span>
        </div>
      </div>
    </div>
  )
}

/**
 * 底部那行诊断信息。
 *
 * 「建 N 次」不是给用户看的业务字段，而是**可被外部核对的证据**：
 * 连打 3 个字符会发 3 次搜索，这个数必须还是 1。把它显示出来，
 * 比在验收脚本里读进程内存便宜得多。
 */
function summaryOf(
  result: SearchResult | null,
  error: string | null,
  busy: boolean,
): string {
  if (error !== null) return '搜索失败'
  if (result === null) return busy ? '搜索中…' : ''
  if (result.total === 0) return '没有匹配的行'

  const parts: string[] = []
  parts.push(
    // 三个状态，别合并：
    // - `truncated`：候选池没数全，`total` 是**下界** → 「至少 N 条」。
    // - `total > hits.length`：数清了，但只返回前 200 条 → 「前 N 条 · 共 M 条」。
    // - 两者都不成立：全都在这儿 → 「N 条」。
    //
    // `truncated` 曾经把前两种混成一个布尔值（命中超过 200 条也算截断），
    // 于是「total 准确但只显示 200 条」也被说成「至少」——准确值被降级成了
    // 下界。现在它是「total 不准」的唯一信号（契约见 `SearchResult::truncated`）。
    result.truncated
      ? `前 ${result.hits.length} 条 · 至少 ${result.total} 条`
      : result.total > result.hits.length
        ? `前 ${result.hits.length} 条 · 共 ${result.total} 条`
        : `${result.hits.length} 条`,
  )
  parts.push(`索引 ${result.indexedFiles} 篇`)
  // 「跳过 M」不在这里：它变成了 footer 上可点的按钮（见上面的渲染）。
  // 一个数字只有变成可点之后，用户才能回答「是哪几篇」——而那正是本刀的全部。
  parts.push(`建索引 ${result.builds} 次`)
  if (result.indexMs > 0) parts.push(`${result.indexMs} ms`)
  return parts.join(' · ')
}
