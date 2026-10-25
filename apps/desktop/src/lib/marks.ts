import type { ChangeSpec } from '@codemirror/state'

/**
 * 行内标记的种类。
 *
 * 只做 `**粗**` 与 `*斜*`（必要时 `_斜_`，见 `CHARS`）。`__粗__` 不在这一刀里：
 * 中文正文里下划线的出现频率远高于它作为定界符的频率，而 CommonMark 对 `_`
 * 的词内限制又比 `*` 苛刻——支持它只会让「为什么没生效」变成常见问题。
 */
export type MarkKind = 'bold' | 'italic'

/**
 * 一次按键的完整结果：**改动与选区必须一起给**。
 *
 * # 为什么不能分两次 dispatch
 *
 * 1. 撤销栈会多一步。用户按一次 `Mod+B`，要按两次 `Mod+Z` 才回到原样——
 *    那是缺陷，不是细节。
 * 2. 第二次 dispatch 时 `to` 是按**旧文档**算出来的。包裹之后它落在新插入的
 *    定界符中间，光标会莫名其妙地偏两个字符。
 *
 * 所以这个返回值就是 `view.dispatch` 的参数，`NoteEditor` 只负责转发。
 */
export interface MarkEdit {
  changes: ChangeSpec
  selection: { anchor: number; head: number }
}

/** 定界符由几个字符组成。加粗两颗星，斜体一颗。 */
const WIDTH: Record<MarkKind, number> = { bold: 2, italic: 1 }

/**
 * 每种标记允许的定界字符，**第一个是默认**。
 *
 * 斜体有两个：`*` 是默认，`_` 只在一个地方用——插入点紧挨着 `*` 的时候。
 *
 * 理由就是验收里那条「不许重排」：`**abc**` 上按斜体，若照常插 `*` 会得到
 * `***abc***`。三颗星在 CommonMark 里虽然能渲染成「加粗套斜体」，但它是
 * **歧义**的：下一次按键认不出哪一颗属于谁，用户也看不出这一按改了什么。
 * 换成 `_` 之后两层各自独立、可逆：`**_abc_**` ↔ `**abc**`。
 *
 * 注意这**不是**「智能嵌套规范化」——我们不改动任何一个已有的星号，
 * 只是给新插入的那一对选了一个不与邻居粘连的字符。
 */
const CHARS: Record<MarkKind, readonly string[]> = {
  bold: ['*'],
  italic: ['*', '_'],
}

/** `'*'` 的码点。逐字符比较比 `doc[i] === '*'` 少一次字符串分配。 */
const STAR = 42

/**
 * 从 `pos` 数连续的 `ch`：`dir = -1` 数左边（不含 `pos`），`dir = 1` 数右边（含）。
 *
 * 「连续同类字符段」是这套算法唯一的语法依据。不去解析整棵树，是因为这一刀
 * 只允许一个纯函数（`doc` 进、改动出），而语法树是异步增量解析的——
 * 用它当判据，同一份文档在解析补全前后会得到两种行为。
 */
function runOf(doc: string, pos: number, dir: -1 | 1, code: number): number {
  let i = pos
  if (dir < 0) {
    while (i > 0 && doc.charCodeAt(i - 1) === code) i -= 1
    return pos - i
  }
  while (i < doc.length && doc.charCodeAt(i) === code) i += 1
  return i - pos
}

/**
 * 一段连续同类字符能不能被当作 `width` 个字符的定界符。
 *
 * 两个条件：
 *
 * 1. 长度至少 `width`（一个 `*` 不能当 `**` 用）。
 * 2. **奇偶性相同**。这一条是「不许自作聪明重排」的另一半：`**abc**` 里
 *    `abc` 两侧是长度为 2 的星号段（偶数），它属于加粗；在它上面按斜体
 *    必须**再包一层**，而不是拆掉其中一半把加粗偷偷改写成斜体。
 *    反过来 `***abc***` 是奇数段，拆一个 `*` 之后剩下的 `**` 仍然是合法的
 *    加粗，所以那里可以拆。
 *
 * 代价是 `***abc***` 上按加粗会再包一层（`*****abc***` 那种退化形态）。
 * 那一格本来就是歧义：三颗星既可以是「加粗套斜体」也可以是「斜体套加粗」，
 * 字符串层面无法区分，宁可什么都不做也不猜。
 */
function runFits(length: number, width: number): boolean {
  return length >= width && length % 2 === width % 2
}

/** ASCII 标识符字符。`_` 的词内限制只认这些，见 `flankOk`。 */
function isIdentChar(code: number): boolean {
  return (code >= 48 && code <= 57) || (code >= 65 && code <= 90) || (code >= 97 && code <= 122)
}

/**
 * `_` 的贴边检查。
 *
 * CommonMark 不把词内的 `_` 当定界符，因为 `snake_case_name` 太常见。
 * 不抄这一条的话，用户选中 `snake_case_name` 里的 `case` 按一下斜体，
 * 我们会把两侧的下划线当成自己的定界符**拆掉**，得到 `snakecasename`——
 * 一次按键毁掉一个变量名，且 `Mod+Z` 之前看不出来。
 *
 * `*` 没有这个限制：CommonMark 允许词内 `*` 强调。
 *
 * @param at 定界符区间的起点
 * @param opening 是开头定界符（看左边一个字符）还是收尾定界符（看右边一个字符）
 */
function flankOk(doc: string, at: number, width: number, opening: boolean): boolean {
  const probe = opening ? at - 1 : at + width
  if (probe < 0 || probe >= doc.length) return true
  return !isIdentChar(doc.charCodeAt(probe))
}

/** 选区在一行内的落点。空行与纯空白行不会出现在这个数组里。 */
interface Segment {
  from: number
  to: number
}

/** 一侧定界符占用的区间。 */
interface Delim {
  from: number
  to: number
}

/** 一侧的定位结果：要删掉的定界符 + 文字真正的边界。 */
interface Side {
  delim: Delim
  content: number
}

/**
 * 找一侧的定界符。**先看选区外面，再看选区里面**。
 *
 * - 外面：`**abc**` 里选中的是 `abc`——这是包完之后最自然的选区，
 *   所以「再按一次拆掉」必须优先认这种。
 * - 里面：用户双击选中 `**abc**` 整段、或三击选中整行时，星号在选区里。
 */
function findSide(
  doc: string,
  pos: number,
  width: number,
  code: number,
  opening: boolean,
): Side | null {
  // 朝选区外的方向：开头定界符在左边，收尾定界符在右边。
  const out = opening ? -1 : 1
  const outer = runOf(doc, pos, out, code)
  if (runFits(outer, width)) {
    const delim = opening
      ? { from: pos - width, to: pos }
      : { from: pos, to: pos + width }
    if (code === STAR || flankOk(doc, delim.from, width, opening)) {
      return { delim, content: pos }
    }
  }
  // 朝选区里的方向：开头定界符在右边，收尾定界符在左边。
  const inner = runOf(doc, pos, opening ? 1 : -1, code)
  if (runFits(inner, width)) {
    const delim = opening
      ? { from: pos, to: pos + width }
      : { from: pos - width, to: pos }
    if (code === STAR || flankOk(doc, delim.from, width, opening)) {
      return { delim, content: opening ? pos + width : pos - width }
    }
  }
  return null
}

/**
 * 一个片段（连同它两侧的定界符）被拆开之后的真实边界。
 *
 * 两侧独立定位是必须的：多行包裹之后选区从第一段文字跨到最后一段文字，
 * 中间几段的收尾星号落进了选区里、而开头星号还在选区外——只要写成
 * 「两侧同时在外」或「两侧同时在内」，第二轮就认不出自己包过的东西，
 * 于是变成再包一层。
 *
 * 但**两侧必须用同一个字符**：`*abc_` 不是任何合法的强调，混着拆只会
 * 把文档拆坏。
 */
interface Located {
  from: number
  to: number
  lead: Delim | null
  trail: Delim | null
}

function locate(doc: string, seg: Segment, width: number, chars: readonly string[]): Located {
  for (const ch of chars) {
    const code = ch.charCodeAt(0)
    const lead = findSide(doc, seg.from, width, code, true)
    const trail = findSide(doc, seg.to, width, code, false)
    // `from > to` 只可能出现在两侧定界符其实是同一段字符时（比如把 `**`
    // 整个选中再按加粗）。那时它不是"已包裹"，交给调用方按未包裹处理。
    if (lead !== null && trail !== null && lead.content <= trail.content) {
      return { from: lead.content, to: trail.content, lead: lead.delim, trail: trail.delim }
    }
  }
  return { from: seg.from, to: seg.to, lead: null, trail: null }
}

/**
 * 包裹时用哪个字符。
 *
 * 默认取候选表的第一项；只有当插入点紧挨着它时，退到第二项。
 * 加粗只有一个候选，所以永远返回 `*`。
 */
function pickChar(doc: string, seg: Segment, chars: readonly string[]): string {
  if (chars.length === 1) return chars[0]
  const first = chars[0].charCodeAt(0)
  const left = seg.from > 0 ? doc.charCodeAt(seg.from - 1) : -1
  const right = seg.to < doc.length ? doc.charCodeAt(seg.to) : -1
  return left === first || right === first ? chars[1] : chars[0]
}

/**
 * 把 `[start, end)` 按行切开，丢掉空行与纯空白行。
 *
 * # 为什么必须按行切
 *
 * 整体包裹跨段落时会产出 `**a\n\nb**`，而它在 CommonMark 里**不是**强调
 * （段落边界会终止行内解析）。用户看到的是「按了加粗，什么也没发生」，
 * 且没有任何提示能解释为什么——所以一行一个定界符对，空行原样跳过。
 *
 * # 为什么只切选区，不扩展到整行
 *
 * 用户选的是哪一段就包哪一段。扩展到整行会让「选中半句话加粗」变成
 * 「整段加粗」，那是另一种自作聪明。
 */
function segmentsOf(doc: string, start: number, end: number): Segment[] {
  const segments: Segment[] = []

  // 回退到 `start` 所在行的行首。逐字符走，而不是 `lastIndexOf('\n', start - 1)`：
  // 后者在 `start === 0` 时会把位置 -1 当作 0，行首算成 1（虽然结果一样，
  // 但那是靠"第一行恰好是空行"兜住的，不该依赖）。
  let lineStart = start
  while (lineStart > 0 && doc.charCodeAt(lineStart - 1) !== 10) lineStart -= 1

  for (;;) {
    const newline = doc.indexOf('\n', lineStart)
    const lineEnd = newline < 0 ? doc.length : newline
    const segFrom = Math.max(start, lineStart)
    const segTo = Math.min(end, lineEnd)
    // 纯空白行也跳过：`**   **` 是合法的强调，但它什么也强调不了，
    // 而多出来的四颗星只会让用户以为按错了。
    if (segTo > segFrom && doc.slice(segFrom, segTo).trim() !== '') {
      segments.push({ from: segFrom, to: segTo })
    }
    if (newline < 0 || newline + 1 > end) break
    lineStart = newline + 1
  }

  return segments
}

/**
 * 算出「按一次加粗 / 斜体」要做的全部改动。
 *
 * # 三条对称语义
 *
 * 1. **空选区**：插入一对空定界符并把光标放在正中间；在正中间再按一次
 *    等于把它删掉、光标回原位。不做「再包一层」。
 * 2. **已包裹**：拆掉定界符，选区仍是原来那段文字（不是整段带星号）。
 * 3. **未包裹**：包一层，选区跟着文字走。
 *
 * # 返回值
 *
 * `null` 表示这一按什么也不该做（选区里全是空行）。调用方据此返回 `false`，
 * 让 CM6 的 keymap 继续往下找——但我们自己的绑定带了 `preventDefault`，
 * 所以 WebView 也不会拿这个键去做别的。
 */
export function toggleMark(
  doc: string,
  from: number,
  to: number,
  mark: MarkKind,
): MarkEdit | null {
  const width = WIDTH[mark]
  const chars = CHARS[mark]
  const start = Math.min(from, to)
  const end = Math.max(from, to)
  /** 反向选择（anchor 在 head 右边）。结果要按原方向返回，否则手感会翻。 */
  const reversed = from > to

  /* ── 空选区：光标在一对空定界符的正中间吗 ── */
  if (start === end) {
    const pos = start
    const unit = chars[0].repeat(width)
    if (pos >= width && doc.slice(pos - width, pos) === unit && doc.slice(pos, pos + width) === unit) {
      const at = pos - width
      return {
        changes: { from: at, to: pos + width, insert: '' },
        selection: { anchor: at, head: at },
      }
    }
    return {
      changes: { from: pos, insert: unit + unit },
      selection: { anchor: pos + width, head: pos + width },
    }
  }

  const segments = segmentsOf(doc, start, end)
  if (segments.length === 0) return null

  /** 全部改动。`insert` 为空串表示删除，否则是插入（`from === to`）。 */
  const edits: { from: number; to: number; insert: string }[] = []

  for (const seg of segments) {
    const spot = locate(doc, seg, width, chars)
    if (spot.lead !== null && spot.trail !== null) {
      edits.push({ from: spot.lead.from, to: spot.lead.to, insert: '' })
      edits.push({ from: spot.trail.from, to: spot.trail.to, insert: '' })
    } else {
      const unit = pickChar(doc, seg, chars).repeat(width)
      edits.push({ from: seg.from, to: seg.from, insert: unit })
      edits.push({ from: seg.to, to: seg.to, insert: unit })
    }
  }

  /**
   * 把旧坐标映射到新文档。**锚点与头用不同的规则**。
   *
   * 规则是「右端点 `<= pos` 的改动计入」，但有一个例外：**正好落在 `pos`
   * 上的插入不计入头**。
   *
   * 为什么：
   *
   * - 锚点要落到开头定界符**之后**，所以那个插入必须计入（`to === from === start`）。
   * - 头要停在收尾定界符**之前**，所以那个插入必须不计入——否则选区会把
   *   `**abc**` 整个吞进去，第二次按就变成「连星号一起选中」的另一条分支，
   *   而多行场景下那条分支会把中间几段拆得七零八落。
   * - 删除两种都正确：拆外部定界符时区间右端正好是 `start`（计入，锚点左移）；
   *   拆内部定界符时区间从 `start` 开始（右端更远，不计入，锚点不动）。
   */
  const shift = (pos: number, countInsertHere: boolean): number => {
    let delta = 0
    for (const edit of edits) {
      if (edit.to > pos) continue
      if (edit.to === pos && edit.from === edit.to && !countInsertHere) continue
      delta += edit.insert.length - (edit.to - edit.from)
    }
    return delta
  }

  const anchor = start + shift(start, true)
  const head = end + shift(end, false)

  return {
    changes: edits,
    selection: reversed ? { anchor: head, head: anchor } : { anchor, head },
  }
}
