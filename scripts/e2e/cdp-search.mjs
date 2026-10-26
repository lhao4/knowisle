// M2 第三刀 · 全文搜索（Cmd/Ctrl+Shift+F）端到端验收。真实 WebView2 + CDP。
//
// 覆盖验收 1–9；验收 10（typecheck / lint / bindings:check / cargo test）
// 是命令行检查，不在本脚本里。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`（vite 在 127.0.0.1:1420，且已用 curl 核对过下发的模块，B-18）
//   2. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16）
//   3. 运行期间源码冻结
//
// 夹具由本脚本自己生成（`prepareQuickVault()`，1000 篇 + GBK + 超大，幂等）。
//
// 跑法：`node scripts/e2e/cdp-search.mjs`
import fs from 'node:fs'
import path from 'node:path'

import {
  APP_ORIGIN,
  CDP_PORT as PORT,
  OUT_DIR,
  QUICK_VAULT as VAULT,
  sleep,
} from './env.mjs'
import { prepareQuickVault } from './fixtures.mjs'

const fixture = prepareQuickVault()
console.log(`夹具：${fixture.count} 篇 + 2 个坏文件 → ${fixture.root}`)

const MAX_INDEX_BYTES = 2 * 1024 * 1024
const TYPED = `KI-TYPED-${Date.now()}`

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  console.log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}
const skip = (name, reason) => {
  results.push({ name, ok: null, detail: reason })
  console.log(`SKIP  ${name}  ${reason}`)
}

/* ───────────────────── 期望值（Node 侧独立扫磁盘） ───────────────────── */

/** ASCII 折叠，必须与 Rust 侧逐字一致：只映射 A-Z，别的都不动。 */
const fold = (text) => text.replace(/[A-Z]/g, (c) => c.toLowerCase())

const walk = (dir, prefix) => {
  const out = []
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const rel = prefix === '' ? entry.name : `${prefix}/${entry.name}`
    if (entry.isDirectory()) out.push(...walk(path.join(dir, entry.name), rel))
    else if (/\.(md|markdown)$/i.test(entry.name)) out.push(rel)
  }
  return out
}

const ALL_PATHS = walk(VAULT, '').sort()

/**
 * 读一篇的正文；读不了返回 null。
 *
 * 这里必须**独立复刻**索引的两条跳过规则（>2MB、非 UTF-8），否则对账时
 * 会把"索引刻意跳过的文件"算成"搜索漏了"。
 */
const contentOf = (rel) => {
  const absolute = path.join(VAULT, rel)
  if (fs.statSync(absolute).size > MAX_INDEX_BYTES) return null
  const bytes = fs.readFileSync(absolute)
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes)
  } catch {
    return null
  }
}

const CONTENT = new Map()
for (const rel of ALL_PATHS) CONTENT.set(rel, contentOf(rel))

const expectedFiles = (query) => {
  const needle = fold(query)
  return ALL_PATHS.filter((rel) => {
    const text = CONTENT.get(rel)
    return text !== null && fold(text).includes(needle)
  }).sort()
}

const sameList = (a, b) => a.length === b.length && a.every((value, i) => value === b[i])

/* ───────────────────────────── CDP ───────────────────────────── */

const targets = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
// 只认 URL 落在 dev 地址上的那个：`/json` 里可能同时有 `about:blank` 的僵尸，
// `find(t => t.type === 'page')` 抓到前者会永远读不到地址。
const page = targets.find((t) => t.type === 'page' && (t.url ?? '').startsWith(APP_ORIGIN))
if (!page) throw new Error('没找到 dev 页面的 target：应用是不是没带 --remote-debugging-port 启动？')

const ws = new WebSocket(page.webSocketDebuggerUrl)
let seq = 0
const waiting = new Map()
ws.addEventListener('message', (e) => {
  const m = JSON.parse(e.data)
  if (m.id && waiting.has(m.id)) {
    waiting.get(m.id)(m)
    waiting.delete(m.id)
  }
})
const send = (method, params = {}) =>
  new Promise((resolve) => {
    const id = ++seq
    waiting.set(id, resolve)
    ws.send(JSON.stringify({ id, method, params }))
  })
await new Promise((r) => ws.addEventListener('open', r))
await send('Runtime.enable')
await send('Page.enable')

const ev = async (expression) => {
  const res = await send('Runtime.evaluate', {
    expression,
    returnByValue: true,
    awaitPromise: true,
  })
  if (res.result?.exceptionDetails) {
    throw new Error(
      res.result.exceptionDetails.exception?.description ??
        res.result.exceptionDetails.text ??
        'evaluate failed',
    )
  }
  return res.result?.result?.value
}

/** 直接调 IPC。**不替换任何成员**（B-15），只调用应用自己的命令。 */
const invoke = (command, args) =>
  ev(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)})`)

const search = (query, refreshPath = null) =>
  invoke('vault_search', { root: VAULT, query, refreshPath })

/** 带页面内计时的搜索：不含 CDP 往返。 */
const timedSearch = (query) =>
  ev(`(async () => {
    const t0 = performance.now()
    const result = await window.__TAURI_INTERNALS__.invoke('vault_search', ${JSON.stringify({
      root: VAULT,
      query,
      refreshPath: null,
    })})
    return { ms: performance.now() - t0, result }
  })()`)

/**
 * 从 DOM 反算 CM6 的 selection 位置。
 *
 * # 为什么不直接读 EditorView
 *
 * `EditorView` 没有挂到任何全局对象上，而替换 `__TAURI_INTERNALS__` 的成员
 * 在 Tauri 2 上是静默失败的（B-15）。原生 selection 是**真实渲染结果**，
 * 从这里反算反而比读内部状态更硬：它同时证明了"位置对"和"选中的是那段文字"。
 *
 * # 长文档的坑
 *
 * CM6 只渲染视口附近的 `.cm-line`，所以「前面所有行的长度之和」只在短文档里
 * 等于文档偏移。行号 gutter 给出的是**文档行号**（视口第一行可能是 372），
 * 用它 + 行内列号，再回到磁盘正文里算偏移，才是与渲染无关的算法。
 */
const HEAD = `(() => {
  const sel = window.getSelection()
  if (sel === null || sel.rangeCount === 0) return null
  const range = sel.getRangeAt(0)
  const node = range.startContainer
  const element = node.nodeType === Node.TEXT_NODE ? node.parentElement : node
  const line = element && element.closest ? element.closest('.cm-line') : null
  if (line === null) return null
  const lines = [...line.parentElement.children].filter((el) => el.classList.contains('cm-line'))
  const domIndex = lines.indexOf(line)
  const numbered = [...document.querySelectorAll('.cm-lineNumbers .cm-gutterElement')].filter(
    (el) => el.offsetHeight > 0 && /^[0-9]+$/.test(el.textContent),
  )
  const docLine =
    numbered.length === lines.length ? Number(numbered[domIndex].textContent) : domIndex + 1
  const before = document.createRange()
  before.setStart(line, 0)
  before.setEnd(node, range.startOffset)
  return {
    line: docLine,
    domLine: domIndex + 1,
    column: before.toString().length,
    text: line.textContent,
    selected: sel.toString(),
  }
})()`

const head = () => ev(HEAD)

/** 文档行号 + 行内列号 → 绝对偏移。用磁盘正文算，与渲染无关。 */
const offsetOf = (rel, line, column) => {
  const lines = CONTENT.get(rel).split('\n')
  let offset = column
  for (let i = 0; i < line - 1; i += 1) offset += lines[i].length + 1
  return offset
}

const pressKey = async (key, code, keyCode, modifiers = 0) => {
  await send('Input.dispatchKeyEvent', {
    type: 'keyDown',
    key,
    code,
    windowsVirtualKeyCode: keyCode,
    modifiers,
  })
  await send('Input.dispatchKeyEvent', {
    type: 'keyUp',
    key,
    code,
    windowsVirtualKeyCode: keyCode,
    modifiers,
  })
}

/** Ctrl+Shift+F（modifiers: 2 = Ctrl, 8 = Shift）。 */
const openSearch = async () => {
  await pressKey('F', 'KeyF', 70, 2 | 8)
  for (let i = 0; i < 60; i += 1) {
    if (await ev(`document.querySelector('.search__input') !== null`)) return true
    await sleep(20)
  }
  return false
}

const waitFor = async (expression, tries = 100, interval = 30) => {
  for (let i = 0; i < tries; i += 1) {
    if (await ev(expression)) return true
    await sleep(interval)
  }
  return false
}

/* ───────────────────────────── 准备 ───────────────────────────── */

await ev(`localStorage.setItem('knowisle.lastVault', ${JSON.stringify(VAULT)})`)
await send('Page.reload', {})
if (!(await waitFor(`document.querySelector('.stats') !== null`, 120, 100))) {
  throw new Error('Vault 没有加载出来')
}
console.log('vault:', await ev(`document.querySelector('.stats').textContent`))

// 打开 foo/bar.md 作为「当前笔记」（foo 目录有 41 个子节点，默认不展开）。
await ev(`[...document.querySelectorAll('button.tree-row--dir')].find((b) => b.title === 'foo')?.click()`)
await sleep(100)
await ev(
  `[...document.querySelectorAll('button.tree-row--file')].find((b) => b.title === 'foo/bar.md')?.click()`,
)
if (!(await waitFor(`document.querySelector('.cm-content') !== null`, 100, 50))) {
  throw new Error('编辑器没有出现')
}

/* ── ① 后台建索引：等就绪，再量热查询 p95 ──────────────────────────── */

// 应用启动时那次 `vault_scan` 已经把索引交给后台线程了（M6 第二刀）。
// 所以这里不再有"首次搜索顺便建库"这回事：第一次搜索只会看到 `building`，
// `builds` 也是 0（后台自开连接，不经过缓存）。要验的是三件事：
//   1. 建库期间搜索能返回（不阻塞，也不去抢着建第二份）；
//   2. 状态最终变成 `ready`，且库是全的（1000 篇）；
//   3. 建库耗时通过 `indexMs` 回填给了下一次搜索——否则这个字段永远是 0。
const readyStart = Date.now()
let state = null
let indexedFiles = 0
let skippedFiles = 0
let reportedIndexMs = 0
for (;;) {
  const one = await search('recipes')
  state = one.indexState
  indexedFiles = one.indexedFiles
  skippedFiles = one.skippedFiles
  reportedIndexMs = Math.max(reportedIndexMs, one.indexMs)
  if (state?.state !== 'building') break
  if (Date.now() - readyStart > 120_000) break
  await sleep(200)
}
check(
  '1.index-ready',
  state?.state === 'ready' && indexedFiles === 1000 && reportedIndexMs > 0,
  {
    state,
    waitMs: Date.now() - readyStart,
    indexMs: reportedIndexMs,
    indexedFiles,
    skippedFiles,
  },
)

const samples = []
for (let i = 0; i < 20; i += 1) {
  const one = await timedSearch('recipes')
  samples.push(one.ms)
}
samples.sort((a, b) => a - b)
const p95 = samples[Math.ceil(samples.length * 0.95) - 1]
const median = samples[Math.floor(samples.length / 2)]
check('1.warm-p95', p95 <= 150, {
  p95: Math.round(p95 * 10) / 10,
  median: Math.round(median * 10) / 10,
  max: Math.round(samples[samples.length - 1] * 10) / 10,
})

/* ── ② 命中集合对账（中文 / 英文 / 混合） ─────────────────────────── */

// 对账分两档，因为 M4 换引擎之后 `total` 的语义变了（见 `SearchResult::total`）：
//   - 结果没被截断 → 全库精确对账，这是最硬的那条；
//   - 结果被截断（`truncated`）→ 只验**没有假阳性**。候选池取满 200 条时
//     `total` 只是下界，"漏了谁"没有契约可依（B-36）。
// 早先这三条按"内存引擎扫全库"写，`笔记 下的第` 这类多词中文查询会永远红。
for (const query of ['这是 笔记 下的第', 'recipes', '笔记 下的第 7']) {
  const expected = expectedFiles(query)
  const got = await search(query)
  const files = [...new Set(got.hits.map((hit) => hit.path))].sort()
  const unexpected = files.filter((rel) => !expected.includes(rel))
  if (got.truncated) {
    check(`2.set:${query}`, unexpected.length === 0, {
      expected: expected.length,
      got: files.length,
      total: got.total,
      truncated: true,
      unexpected: unexpected.slice(0, 5),
    })
  } else {
    check(`2.set:${query}`, sameList(expected, files), {
      expected: expected.length,
      got: files.length,
      total: got.total,
    })
  }
}

/* ── ③ 命中行号与原文逐字一致 ─────────────────────────────────────── */

const recipes = await search('recipes')
const sampled = recipes.hits.slice(0, 5)
const lineChecks = sampled.map((hit) => {
  const lines = CONTENT.get(hit.path).split('\n')
  return {
    path: hit.path,
    line: hit.line,
    ok: lines[hit.line - 1] === hit.text,
    disk: lines[hit.line - 1],
    got: hit.text,
  }
})
check('3.line-text', lineChecks.every((item) => item.ok), lineChecks.map((i) => `${i.path}:${i.line}`))

/* ── ④ 大小写 / 中文子串 / 全角半角 ───────────────────────────────── */

const alpha = await search('alpha')
// 每篇 alpha 笔记有两行含 alpha：标题 `# Alpha-XXXX` 与正文「这是 alpha 下的…」。
// 断言按**文件集合**算，再单独盯住标题行——那才是「小写查询命中大写原文」的证据。
const alphaFiles = new Set(alpha.hits.map((hit) => hit.path))
const titleHits = alpha.hits.filter((hit) => hit.line === 1)
check(
  '4.ascii-case',
  alphaFiles.size === 40 &&
    titleHits.length === 39 &&
    titleHits.every((hit) => hit.text.slice(hit.from, hit.to) === 'Alpha'),
  { files: alphaFiles.size, titleHits: titleHits.length, total: alpha.total },
)

const half = await search('ABC')
check(
  '4.half-width',
  half.hits.length === 1 && half.hits[0].column === 12 &&
    half.hits[0].text.slice(half.hits[0].from, half.hits[0].to) === 'ABC',
  { hits: half.hits.length, column: half.hits[0]?.column, slice: half.hits[0]?.text.slice(half.hits[0].from, half.hits[0].to) },
)

const full = await search('ＡＢＣ')
check(
  '4.full-width',
  full.hits.length === 1 && full.hits[0].column === 5 &&
    full.hits[0].text.slice(full.hits[0].from, full.hits[0].to) === 'ＡＢＣ',
  { hits: full.hits.length, column: full.hits[0]?.column },
)

const chinese = await search('样本笔记')
// `total` 是**候选池内**的命中行数，池子取满 200 条时它是下界而不是全库计数。
// 1000 篇都含 `样本笔记`，所以这里能验的只有三件事：截断标记为真、命中行
// 铺满 200 条、下界不小于上限。要精确数到 1000 就得读全库——contentless
// 表不存正文，那正是 `truncated` 存在的理由（B-36）。
check(
  '4.chinese+cap',
  chinese.truncated && chinese.hits.length === 200 && chinese.total >= 200,
  {
    total: chinese.total,
    shown: chinese.hits.length,
    truncated: chinese.truncated,
  },
)

/* ── ⑤ 点击结果 → selection.head 与视口 ───────────────────────────── */

const longNote = await search('长文探针在此')
const longHit = longNote.hits[0]
if (longHit === undefined) {
  check('5.click-jump', false, '没有找到长文探针')
} else {
  // 用真实浮层走一遍：快捷键 → 输入 → 点第一项。
  const opened = await openSearch()
  if (!opened) {
    check('5.click-jump', false, '搜索浮层没有打开')
  } else {
    await send('Input.insertText', { text: '长文探针在此' })
    if (!(await waitFor(`document.querySelector('.search__item') !== null`, 120, 30))) {
      check('5.click-jump', false, '列表没有出现')
    } else {
      await ev(`document.querySelector('.search__item').click()`)
      // 跨文件：等新文档挂载并把光标放好。
      await waitFor(
        `document.querySelector('.search-panel') === null && document.querySelector('.cm-content')?.innerText.includes('长文探针在此')`,
        120,
        30,
      )
      await sleep(150)
      const got = await head()
      const expected = CONTENT.get('notes/foobar.md').indexOf('长文探针在此')
      const actual = got === null ? null : offsetOf('notes/foobar.md', got.line, got.column)
      const viewport = await ev(`(() => {
        const scroller = document.querySelector('.cm-scroller')
        const line = [...document.querySelectorAll('.cm-line')].find((el) => el.textContent === '长文探针在此')
        if (!line) return { found: false }
        const a = line.getBoundingClientRect()
        const b = scroller.getBoundingClientRect()
        return { found: true, visible: a.top >= b.top - 1 && a.bottom <= b.bottom + 1, scrollTop: scroller.scrollTop }
      })()`)
      check(
        '5.click-jump',
        actual === expected &&
          got?.selected === '长文探针在此' &&
          viewport.visible &&
          viewport.scrollTop > 0,
        {
          expected,
          head: actual,
          line: got?.line,
          column: got?.column,
          selected: got?.selected,
          visible: viewport.visible,
          scrollTop: Math.round(viewport.scrollTop),
        },
      )
    }
  }
}

/* ── ⑥ 刚打的字立刻能搜到 ─────────────────────────────────────────── */

// 光标放到长文探针那一行的行尾（check 5 已经把它滚进视口）。
// 不假设 5 一定成功：先点一下真实 DOM，确保焦点与光标都在编辑器里。
await ev(`(() => {
  const line = [...document.querySelectorAll('.cm-line')].find((el) => el.textContent === '长文探针在此') ?? document.querySelector('.cm-line')
  const r = line.getBoundingClientRect()
  return { x: r.right - 4, y: r.top + r.height / 2 }
})()`).then(async (point) => {
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', x: point.x, y: point.y, button: 'left', clickCount: 1 })
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: point.x, y: point.y, button: 'left', clickCount: 1 })
})
await sleep(120)
await pressKey('End', 'End', 35)
await send('Input.insertText', { text: ` ${TYPED}` })
const typedOpened = await openSearch()
if (!typedOpened) {
  check('6.typed-visible', false, '搜索浮层没有打开')
} else {
  await send('Input.insertText', { text: TYPED })
  const appeared = await waitFor(
    `[...document.querySelectorAll('.search__item')].some((li) => li.dataset.path === 'notes/foobar.md')`,
    120,
    30,
  )
  const onDisk = fs.readFileSync(path.join(VAULT, 'notes', 'foobar.md'), 'utf8').includes(TYPED)
  check('6.typed-visible', appeared && onDisk, { appeared, onDisk, typed: TYPED })
  await pressKey('Escape', 'Escape', 27)
  await sleep(100)
}

/* ── ⑦ 坏文件不连坐 ───────────────────────────────────────────────── */

const again = await search('recipes')
check(
  '7.skipped-count',
  again.indexedFiles === 1000 && again.skippedFiles === 2 && again.total > 0,
  { indexedFiles: again.indexedFiles, skippedFiles: again.skippedFiles, total: again.total },
)
const gbk = await search('gbk-only-marker')
const huge = await search('huge-only-marker')
check('7.bad-not-indexed', gbk.total === 0 && huge.total === 0, {
  gbk: gbk.total,
  huge: huge.total,
})

/* ── ⑧ Esc 后焦点回编辑器，光标不变 ───────────────────────────────── */

// 把光标放到当前视口中间的一行，记录位置；开浮层、打字、Esc，再比对。
const activePath = await ev(`document.querySelector('.breadcrumb')?.textContent`)
const clickMiddleLine = async () => {
  const point = await ev(`(() => {
    const lines = [...document.querySelectorAll('.cm-line')].filter((el) => el.textContent.trim() !== '')
    const line = lines[Math.floor(lines.length / 2)]
    const r = line.getBoundingClientRect()
    return { x: r.left + 30, y: r.top + r.height / 2 }
  })()`)
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', x: point.x, y: point.y, button: 'left', clickCount: 1 })
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: point.x, y: point.y, button: 'left', clickCount: 1 })
  await sleep(150)
}
await clickMiddleLine()
let beforeEsc = await head()
if (beforeEsc === null) {
  // 点到行尾之外时锚点可能落在 `.cm-content` 上；换个更靠左的位置再来一次。
  await clickMiddleLine()
  beforeEsc = await head()
}
const escOpened = await openSearch()
if (!escOpened) {
  check('8.esc-focus', false, '搜索浮层没有打开')
} else {
  await send('Input.insertText', { text: 'abc' })
  await sleep(80)
  await pressKey('Escape', 'Escape', 27)
  await sleep(200)
  const afterEsc = await head()
  const focused = await ev(`document.activeElement?.classList.contains('cm-content') === true`)
  const beforeOffset =
    beforeEsc === null ? null : offsetOf(activePath, beforeEsc.line, beforeEsc.column)
  const afterOffset =
    afterEsc === null ? null : offsetOf(activePath, afterEsc.line, afterEsc.column)
  check(
    '8.esc-focus',
    focused && beforeOffset !== null && beforeOffset === afterOffset && afterEsc?.selected === '',
    { focused, path: activePath, before: beforeOffset, after: afterOffset, beforeRaw: beforeEsc },
  )
}

/* ── ⑨ 后台建库期间的并发搜索 ─────────────────────────────────────── */

// `vault_scan` 的语义是"重新看一眼磁盘"，它现在把索引交给后台线程。紧接着
// 5 个搜索并发冲进去：没有一个应该去建库（否则就是 5 份索引），也没有一个
// 应该等后台（否则"树先出来"就白做了）。两者的证据分别是 `builds` 全部相同
// 和墙钟时间——这条用例同时也是"搜索不被建库挡住"的并发版。
await invoke('vault_scan', { root: VAULT })
const call = `window.__TAURI_INTERNALS__.invoke('vault_search', ${JSON.stringify({
  root: VAULT,
  query: 'recipes',
  refreshPath: null,
})})`
const concurrentStart = Date.now()
const concurrent = await ev(`Promise.all([${Array(5).fill(call).join(',')}])`)
const wall = Date.now() - concurrentStart
const builds = concurrent.map((one) => one.builds)
const states = concurrent.map((one) => one.indexState?.state)
check(
  '9.concurrent-during-build',
  wall < 1000 &&
    builds.every((value) => value === builds[0]) &&
    states.every((value) => value === states[0]) &&
    concurrent.every((one) => one.total === concurrent[0].total),
  { wallMs: wall, builds, states, total: concurrent[0].total },
)

/* ───────────────────────────── 汇总 ───────────────────────────── */

const passed = results.filter((r) => r.ok === true).length
const failed = results.filter((r) => r.ok === false).length
const skipped = results.filter((r) => r.ok === null).length
console.log(
  `\n汇总：${passed} 通过 / ${failed} 失败 / ${skipped} 跳过` +
    (skipped > 0 ? '  ⚠ 有项目没有执行，本次验收不完整，不能当全绿看' : ''),
)
fs.writeFileSync(
  path.join(OUT_DIR, 'search-acceptance.json'),
  JSON.stringify({ results, samples }, null, 2),
)
ws.close()
process.exit(failed > 0 ? 1 : skipped > 0 ? 2 : 0)
