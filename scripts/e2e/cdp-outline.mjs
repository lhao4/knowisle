// M2 大纲面板的端到端验收（真实 WebView2，CDP）。
//
// 覆盖验收 1–4：
//   1. 六种标题：大纲 6 项、缩进正确、文本逐字一致
//   2. 光标在 H3 段落下 → 第 3 项高亮；点 H5 → 光标落到该行且该行滚到顶部
//   3. 改标题文字 → 大纲实时更新；连打 100 字符 → React 渲染次数 ≈ 标题变化次数
//   4. 切走再切回：大纲正确重建，不残留上一篇
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`：vite 在 127.0.0.1:1420（脚本会取 /src/App.tsx 核对转译结果，B-18）
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16：调试槽位是进程级静态）
//      或 `pnpm e2e:outline --restart` 让脚本自己杀掉并重启
//   4. 运行期间源码冻结
//
// 夹具：`os.tmpdir()/knowisle-e2e-outline`，每次重建（B-32）。之前它靠
// `.tmp/vault-m2` 里的四篇兄弟文件（switch-a / switch-b / plain / long），
// 那些文件没有任何地方记录——入库后必须自己造。
//
// 第 3 条里的「渲染次数」依赖源码里的临时 `console.count` 插桩。没有插桩时
// 那几条走显式 SKIP、退出码 2（B-19），**不会**静默算作通过。
import fs from 'node:fs'
import path from 'node:path'

import { openApp, killApp } from './cdp.mjs'
import { OUT_DIR, OUTLINE_VAULT as VAULT, sleep } from './env.mjs'
import { resetVault } from './fixtures.mjs'

const SIX = path.join(VAULT, 'six.md')

// 每次都重写夹具：上一轮验收会往 six.md 里塞 100 个 a 和一个 Z。
const SIX_SOURCE = `# 一级标题 Alpha

一级标题下面的正文。这一段用来确认「光标落在某个标题的正文里时，高亮的是该标题」，
而不是上一个标题。

## 二级标题 Bravo

二级标题下面的正文。本地优先意味着磁盘上的 Markdown 是唯一真相源，
索引只是可重建的派生数据。

### 三级标题 Charlie

三级标题下面的正文，验收第 2 条把光标停在这里，大纲的第 3 项必须高亮。
这一节刻意写长一点，让 H5 落在需要滚动才能看到的位置。

知识管理的第一步不是收集，而是丢弃。收集是廉价的，丢弃是昂贵的，
因为丢弃要求你先判断什么值得留下。

一个笔记系统最容易失败的地方不是「记不下来」，而是「记下来之后再也不看」。
所以检索路径比输入路径更重要。

这一段只是为了撑高度，让后面的标题离视口足够远。

- 列表项一
- 列表项二
- 列表项三

再补一段正文，确保 H4 与 H3 之间有足够的垂直距离。

#### 四级标题 Delta

四级标题下面的正文。这一段用来确认四级标题的缩进比三级深一档。

> 引用块里的内容不属于标题，但它的行会被挂上引用块的左边线。
> 这一行只是为了让编辑器里有非标题的结构。

##### 五级标题 Echo

五级标题下面的正文。验收第 2 条会点击「五级标题 Echo」，
光标必须落到这一行，并且这一行滚到视口顶部。

如果滚动没有发生，说明 \`scrollIntoView(pos, { y: 'start' })\` 被写成了默认的
\`nearest\`——那只会保证可见，看起来像是「点了没反应」。

###### 六级标题 Foxtrot

六级标题下面的正文。六级是最小的一档字号，但它仍然必须出现在大纲里，
并且与正文逐字一致。

收尾段落。

这一段之后是纯填充，没有任何标题。存在的唯一理由是让「五级标题 Echo」
下方还有足够的高度——否则把它滚到视口顶部时会被文档末尾卡住，
scrollIntoView 只能停在最大滚动位置，验收 2 的判据就失去意义。

填充段落一。填充段落一。填充段落一。填充段落一。填充段落一。填充段落一。
填充段落一。填充段落一。填充段落一。填充段落一。填充段落一。填充段落一。

填充段落二。填充段落二。填充段落二。填充段落二。填充段落二。填充段落二。
填充段落二。填充段落二。填充段落二。填充段落二。填充段落二。填充段落二。

填充段落三。填充段落三。填充段落三。填充段落三。填充段落三。填充段落三。
填充段落三。填充段落三。填充段落三。填充段落三。填充段落三。填充段落三。

填充段落四。填充段落四。填充段落四。填充段落四。填充段落四。填充段落四。
填充段落四。填充段落四。填充段落四。填充段落四。填充段落四。填充段落四。

填充段落五。填充段落五。填充段落五。填充段落五。填充段落五。填充段落五。
填充段落五。填充段落五。填充段落五。填充段落五。填充段落五。填充段落五。

填充段落六。填充段落六。填充段落六。填充段落六。填充段落六。填充段落六。
填充段落六。填充段落六。填充段落六。填充段落六。填充段落六。填充段落六。

填充段落七。填充段落七。填充段落七。填充段落七。填充段落七。填充段落七。
填充段落七。填充段落七。填充段落七。填充段落七。填充段落七。填充段落七。

填充段落八。填充段落八。填充段落八。填充段落八。填充段落八。填充段落八。
填充段落八。填充段落八。填充段落八。填充段落八。填充段落八。填充段落八。
`
/** 兄弟夹具：切走切回用的两篇、空大纲、超长标题（溢出才验得到省略号）。 */
const SIBLINGS = {
  'switch-a.md': '# 甲篇的一级标题\n\n正文。\n\n## 甲篇的二级标题\n\n正文。\n\n### 甲篇的三级标题\n\n正文。\n',
  'switch-b.md': '# 乙篇只有一个标题\n\n正文。\n',
  'plain.md': '只有正文，没有任何标题。\n\n第二段。\n',
  'long.md': `# ${'超长标题'.repeat(30)}\n\n正文。\n`,
}

const buildFixture = () =>
  resetVault(VAULT, { 'six.md': SIX_SOURCE, ...SIBLINGS }, { clearTrash: true })


/* ───────────────────────────── 会话 ───────────────────────────── */

/** 渲染计数事件（`console.count` 的每一次调用）。 */
const counts = []
const onEvent = (m) => {
  if (m.method !== 'Runtime.consoleAPICalled') return
  const args = m.params.args ?? []
  // `console.count` 的 label 会被浏览器格式化成 "label: N"，去掉后缀才是标签。
  const raw = typeof args[0]?.value === 'string' ? args[0].value : ''
  const label = raw.replace(/: \d+$/, '')
  // 白名单必须和插桩点一一对应。**漏一个前缀的后果是「计数器恒为 0」**，
  // 而 0 恰好是「引用稳定」的期望值——静默通过就是这么来的
  // （第一版只收了 KI_RENDER_，KI_ITEMS_NEW 被丢掉，于是 items 那条假绿）。
  if (label === 'KI_ITEMS_NEW' || label.startsWith('KI_RENDER_')) {
    counts.push({ label, n: Number(/: (\d+)$/.exec(raw)?.[1] ?? 0) })
  }
}

const consoleErrors = []
let app
let send
let ev
let waitFor
let clickAt

const bind = (session) => {
  app = session
  send = session.send
  ev = session.ev
  waitFor = session.waitFor
  clickAt = session.clickAt
}

bind(await openApp({ vault: VAULT, prepare: buildFixture, errors: consoleErrors, onEvent }))

/**
 * 标记一个时间点，之后用 `rendersSince(mark)` 统计这一段的渲染次数。
 *
 * dev 构建下 `main.tsx` 开了 `<StrictMode>`，组件函数每次逻辑渲染会被调用两次。
 * 所以 console.count 的**事件数 = 逻辑渲染数 × 2**。这里显式换算，
 * 免得报告里出现「一次按键渲染两次」这种看起来像 bug 的数字。
 */
const STRICT_MULTIPLIER = 2
const mark = () => counts.length
const rendersSince = (from) => {
  const slice = counts.slice(from)
  const tally = {}
  for (const e of slice) tally[e.label] = (tally[e.label] ?? 0) + 1
  return {
    total: slice.length,
    outlineCalls: tally.KI_RENDER_OUTLINE ?? 0,
    outlineRenders: (tally.KI_RENDER_OUTLINE ?? 0) / STRICT_MULTIPLIER,
    appCalls: tally.KI_RENDER_APP ?? 0,
    appRenders: (tally.KI_RENDER_APP ?? 0) / STRICT_MULTIPLIER,
    /** `items` 这个 prop 的**引用**变了几次（= setOutline 传了新数组）。 */
    itemsNew: tally.KI_ITEMS_NEW ?? 0,
  }
}

/** 点某一行（按文本前缀找）。先把行滚进视口再点，否则坐标落在窗口外。 */
const clickLine = async (prefix) => {
  const found = await ev(`(() => {
    const line = [...document.querySelectorAll('.cm-line')]
      .find((l) => l.textContent.startsWith(${JSON.stringify(prefix)}))
    if (!line) return false
    line.scrollIntoView({ block: 'center' })
    return true
  })()`)
  if (!found) throw new Error(`行不在文档里：${prefix}`)
  await sleep(250)
  const point = await ev(`(() => {
    const line = [...document.querySelectorAll('.cm-line')]
      .find((l) => l.textContent.startsWith(${JSON.stringify(prefix)}))
    if (!line) return null
    const r = line.getBoundingClientRect()
    return { x: r.left + 6, y: r.top + r.height / 2 }
  })()`)
  if (point === null) throw new Error(`行不在视口里：${prefix}`)
  await clickAt(point.x, point.y)
  await sleep(120)
}

const openNote = async (name) => {
  const ok = await ev(`(() => {
    const row = [...document.querySelectorAll('.tree-row')]
      .find((r) => r.innerText.includes(${JSON.stringify(name)}))
    if (!row) return false
    row.click()
    return true
  })()`)
  if (!ok) throw new Error(`树里没有 ${name}`)
  await sleep(900)
}

const outline = () =>
  ev(`[...document.querySelectorAll('.outline__item')].map((el) => ({
    text: el.textContent,
    level: Number(el.dataset.level),
    indent: getComputedStyle(el).paddingLeft,
    title: el.getAttribute('title'),
    active: el.classList.contains('is-active'),
  }))`)

const results = {}
const fail = []
const skipped = []

const check = (name, ok, detail) => {
  results[name] = { ok, detail }
  if (!ok) fail.push(`${name}: ${JSON.stringify(detail)}`)
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}

/**
 * 显式跳过。
 *
 * # 为什么不能只是 `if (条件) check(...)`
 *
 * 那样跳过项**根本不进 `results`**，脚本照样 exit 0、打印「全部通过」——
 * 读者无从分辨「验过了」和「压根没跑」。验收脚本最危险的失败模式不是
 * 报错，而是**看起来全绿**。所以跳过必须：打一行 `SKIP`、进 `results`
 * （`ok: null`）、进汇总、并且让退出码变成 2（区别于 1 = 失败）。
 */
const skip = (name, reason) => {
  results[name] = { ok: null, skipped: true, reason }
  skipped.push({ name, reason })
  console.log(`SKIP  ${name}  ${reason}`)
}

// ── 打开库与六标题笔记 ─────────────────────────────────────────────────
// 库已经由 openApp 通过「上次打开的库」装上了。这里再 reload 一次，是为了
// 把计数缓冲清干净——**必须在 reload 之前清**：`Runtime.enable` 会把 target
// 上已有的 console 消息**重放**一遍，上一轮（带插桩时）的 `KI_RENDER_*`
// 会原样喂进来。不清的话，即使源码里已经没有 console.count，
// `counts.length > 0` 依然成立，于是脚本判定「有插桩」，`outlineCalls === 0`
// 又恰好成立，**静默通过**。这就是「验收漏洞」的具体形态：不是报错，是看起来全绿。
counts.length = 0
await app.reload()
await sleep(2600)
await openNote('six.md')

// 渲染计数依赖源码里的临时插桩（console.count）。没插桩时那几条会走
// `skip()`——显式 SKIP、进汇总、退出码 2，**不会**静默算作通过。
const instrumented = counts.length > 0
console.log(`渲染计数插桩：${instrumented ? '有' : '无'}`)

// ── ① 六种标题 ────────────────────────────────────────────────────────
const expected = SIX_SOURCE.split('\n')
  .map((line) => /^(#{1,6}) (.*)$/.exec(line))
  .filter((m) => m !== null)
  .map((m) => ({ level: m[1].length, text: m[2] }))

const items = await outline()
check('1.count', items.length === expected.length, { got: items.length, want: expected.length })
check(
  '1.text',
  items.every((it, i) => it.text === expected[i]?.text && it.title === it.text),
  { got: items.map((i) => i.text), want: expected.map((e) => e.text) },
)
check(
  '1.level',
  items.every((it, i) => it.level === expected[i]?.level),
  items.map((i) => i.level),
)
const indents = items.map((i) => Number.parseFloat(i.indent))
check(
  '1.indent',
  indents.every((v, i) => i === 0 || v > indents[i - 1]),
  indents,
)

// ── ② 光标在 H3 段落下 → 第 3 项高亮 ─────────────────────────────────
await clickLine('三级标题下面的正文')
const afterH3 = await outline()
check(
  '2.active-h3',
  afterH3[2]?.active === true && afterH3.filter((i) => i.active).length === 1,
  afterH3.map((i) => i.active),
)

// ── ② 点 H5 → 光标落到该行且该行滚到顶部 ─────────────────────────────
const h5 = await ev(`(() => {
  const el = [...document.querySelectorAll('.outline__item')]
    .find((e) => e.textContent === '五级标题 Echo')
  if (!el) return null
  const r = el.getBoundingClientRect()
  return { x: r.left + 20, y: r.top + r.height / 2 }
})()`)
if (h5 === null) throw new Error('大纲里没有「五级标题 Echo」')
await clickAt(h5.x, h5.y)
await sleep(400)

const jump = await ev(`(() => {
  const scroller = document.querySelector('.cm-scroller')
  const line = document.querySelector('.cm-activeLine')
  if (!line) return { error: '没有 activeLine' }
  const lr = line.getBoundingClientRect()
  const sr = scroller.getBoundingClientRect()
  return {
    activeText: line.textContent,
    scrollTop: Math.round(scroller.scrollTop),
    // 相对滚动容器顶边的偏移：y:'start' 应当把它压到 0 附近
    // （.cm-content 有 24px padding，所以留一点余量）。
    offsetFromTop: Math.round(lr.top - sr.top),
    focused: document.activeElement?.className ?? '',
  }
})()`)
check(
  '2.jump-line',
  jump.activeText?.startsWith('##### 五级标题 Echo') === true,
  jump.activeText,
)
check(
  '2.jump-scroll-top',
  jump.scrollTop > 100 && jump.offsetFromTop >= -2 && jump.offsetFromTop <= 40,
  { scrollTop: jump.scrollTop, offsetFromTop: jump.offsetFromTop },
)
check('2.jump-focus', jump.focused.includes('cm-content'), jump.focused)
check(
  '2.active-h5',
  (await outline()).filter((i) => i.active).length === 1 &&
    (await outline())[4]?.active === true,
  (await outline()).map((i) => i.active),
)

// ── ③ 连打 100 个字符：渲染次数必须与「标题集合变化次数」同阶 ─────────
// 光标放到 H3 的正文段落里（不是标题行），然后打 100 个 a。
await clickLine('知识管理的第一步不是收集')
const typingMark = mark()
for (let i = 0; i < 100; i += 1) {
  await send('Input.insertText', { text: 'a' })
  await sleep(15)
}
await sleep(900) // 让 300ms 合并窗口与 500ms 状态轮询都跑完
const typingRenders = rendersSince(typingMark)
const typed = await ev(`document.querySelector('.cm-content').innerText.includes('aaaa')`)
check('3.typed-100', typed === true, typed)
if (instrumented) {
  check('3.render-count', typingRenders.outlineCalls === 0, typingRenders)
  // 单独立一条：即使渲染次数对了，也要确认「对」的原因是对的那个。
  // items 引用每按键都换的话，memo 只是把重渲染推迟到下一次，等于装饰。
  check('3.items-ref-stable', typingRenders.itemsNew === 0, { itemsNew: typingRenders.itemsNew })
} else {
  skip('3.render-count', '源码里没有 console.count 插桩')
  skip('3.items-ref-stable', '源码里没有 console.count 插桩')
}

// ── ③ 改标题文字 → 大纲实时更新 ──────────────────────────────────────
await clickLine('# 一级标题 Alpha')
await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'End', code: 'End', windowsVirtualKeyCode: 35 })
await send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'End', code: 'End', windowsVirtualKeyCode: 35 })
await sleep(80)
const editMark = mark()
await send('Input.insertText', { text: 'Z' })
await sleep(400)
const afterEdit = await outline()
const editRenders = rendersSince(editMark)
check(
  '3.heading-updates',
  afterEdit[0]?.text === '一级标题 AlphaZ',
  afterEdit[0]?.text,
)
if (instrumented) {
  check('3.heading-render-count', editRenders.outlineRenders === 1, editRenders)
  check('3.heading-items-ref', editRenders.itemsNew === 1, { itemsNew: editRenders.itemsNew })
} else {
  skip('3.heading-render-count', '源码里没有 console.count 插桩')
  skip('3.heading-items-ref', '源码里没有 console.count 插桩')
}

// ── ④ 切走再切回 ──────────────────────────────────────────────────────
await openNote('switch-a.md')
const a1 = (await outline()).map((i) => i.text)
await openNote('switch-b.md')
const b = (await outline()).map((i) => i.text)
await openNote('switch-a.md')
const a2 = (await outline()).map((i) => i.text)

check('4.a-first', a1.length === 3 && a1[0] === '甲篇的一级标题', a1)
check('4.b-only-one', b.length === 1 && b[0] === '乙篇只有一个标题', b)
check('4.a-again', a2.length === 3 && a2[0] === '甲篇的一级标题', a2)
check('4.no-residue', !a2.includes('乙篇只有一个标题') && !b.includes('甲篇的一级标题'), { a2, b })

// ── 附：空大纲与超长标题 ──────────────────────────────────────────────
await openNote('plain.md')
const empty = await ev(`(() => {
  const el = document.querySelector('.outline__empty')
  return { text: el?.textContent ?? null, items: document.querySelectorAll('.outline__item').length }
})()`)
check('5.empty-state', empty.text === '本篇没有标题' && empty.items === 0, empty)

await openNote('long.md')
const long = await ev(`(() => {
  const el = document.querySelector('.outline__item')
  if (!el) return null
  const cs = getComputedStyle(el)
  return {
    text: el.textContent,
    title: el.getAttribute('title'),
    textOverflow: cs.textOverflow,
    whiteSpace: cs.whiteSpace,
    overflowX: el.scrollWidth > el.clientWidth,
  }
})()`)
check(
  '5.long-title',
  long !== null &&
    long.title === long.text &&
    long.textOverflow === 'ellipsis' &&
    long.whiteSpace === 'nowrap' &&
    long.overflowX === true,
  long === null ? null : { ...long, text: `${long.text.slice(0, 12)}…` },
)


console.log('\n渲染计数（全过程）：', JSON.stringify(counts.reduce((acc, e) => {
  acc[e.label] = (acc[e.label] ?? 0) + 1
  return acc
}, {})))

const passed = Object.values(results).filter((r) => r.ok === true).length
console.log(JSON.stringify({ passed, skipped: skipped.length, failed: fail.length, results }, null, 2))
console.log(`\n通过 ${passed} · 跳过 ${skipped.length} · 失败 ${fail.length}`)
if (skipped.length > 0) {
  console.error(`⚠ ${skipped.length} 项**没有执行**（见上面 SKIP 行）：本次验收不完整，不能当全绿看。`)
}
if (fail.length > 0) {
  console.error(`${fail.length} 项未通过`)
  process.exitCode = 1
} else if (skipped.length > 0) {
  // 2 = 不完整，和 1 = 失败分开：CI 里能一眼看出是「没跑」而不是「跑挂了」。
  process.exitCode = 2
}

fs.writeFileSync(
  path.join(OUT_DIR, `outline-acceptance-${Date.now()}.json`),
  JSON.stringify({ results, counts, stamp: Date.now() }, null, 2),
)
app.close()
killApp()
