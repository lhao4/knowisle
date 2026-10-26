// M2 第二刀 · 快速打开（Cmd/Ctrl+P）的端到端验收。真实 WebView2 + CDP。
//
// 覆盖验收 1–7（验收 8「关窗落盘」由 cdp-quick-close.mjs 单独跑，
// 验收 9 是命令行检查）。每条判据都打印实测值。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`（vite 在 127.0.0.1:1420，且已用 curl 核对过下发的模块，B-18）
//   2. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16）
//   3. 运行期间源码冻结
//
// 夹具由本脚本自己生成（`prepareQuickVault()`，1000 篇，幂等），不再依赖
// 手工先跑一个生成脚本——那正是 B-33 要消灭的"资产在 `.tmp/` 里"。
//
// 跑法：`node scripts/e2e/cdp-quickopen.mjs`
import fs from 'node:fs'
import path from 'node:path'

import { APP_ORIGIN, CDP_PORT as PORT, QUICK_VAULT as VAULT, sleep } from './env.mjs'
import { prepareQuickVault } from './fixtures.mjs'

const fixture = prepareQuickVault()
console.log(`夹具：${fixture.count} 篇 + 2 个坏文件 → ${fixture.root}`)

const NOTE_A = path.join(VAULT, 'notes', 'note-0000.md')
const NOTE_B = 'foo/bar.md'

/* ───────────────────── 期望值（Node 侧独立算一遍） ───────────────────── */

const walk = (dir, prefix) => {
  const out = []
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const rel = prefix === '' ? entry.name : `${prefix}/${entry.name}`
    if (entry.isDirectory()) out.push(...walk(path.join(dir, entry.name), rel))
    else if (entry.name.toLowerCase().endsWith('.md')) out.push(rel)
  }
  return out
}
const ALL_PATHS = walk(VAULT, '').sort()
const EXPECTED_TOP50 = ALL_PATHS.slice(0, 50)

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
  const res = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true })
  if (res.result?.exceptionDetails) {
    throw new Error(res.result.exceptionDetails.exception?.description ?? 'evaluate failed')
  }
  return res.result?.result?.value
}

/**
 * 在**页面任何脚本之前**注入的探针：统计 `window` 上 `keydown` 监听器的
 * 注册/注销次数与调用次数。
 *
 * # 为什么必须用 addScriptToEvaluateOnNewDocument
 *
 * 应用在模块求值阶段就可能注册监听。用 `Runtime.evaluate` 之后再装探针，
 * 早于探针的那次注册永远看不到——而那正是 StrictMode 双挂载的第一份。
 *
 * # 为什么要 unwrap
 *
 * 包装过的监听器与原始函数不是同一个引用，`removeEventListener(orig)` 会
 * 静默失败，于是「清理过了」的监听器仍然在跑。这个 Map 把包装函数换回
 * 原引用，才能让 `remove` 真的生效——否则探针自己就会造出「双监听」的假象。
 */
const TALLY = `(() => {
  const add = EventTarget.prototype.addEventListener
  const remove = EventTarget.prototype.removeEventListener
  const reg = []
  window.__KI_KD__ = reg
  window.__KI_KD_INVOKED__ = 0
  EventTarget.prototype.addEventListener = function (type, fn, opts) {
    if (this === window && type === 'keydown' && typeof fn === 'function' && fn.__kiTest !== true) {
      const wrapped = function (e) {
        window.__KI_KD_INVOKED__ += 1
        return fn.call(this, e)
      }
      wrapped.__orig = fn
      reg.push({ orig: fn, wrapped })
      return add.call(this, type, wrapped, opts)
    }
    return add.call(this, type, fn, opts)
  }
  EventTarget.prototype.removeEventListener = function (type, fn, opts) {
    if (this === window && type === 'keydown') {
      for (let i = 0; i < reg.length; i += 1) {
        if (reg[i].orig === fn) {
          const w = reg[i].wrapped
          reg.splice(i, 1)
          return remove.call(this, type, w, opts)
        }
      }
    }
    return remove.call(this, type, fn, opts)
  }
})()`

await send('Page.addScriptToEvaluateOnNewDocument', { source: TALLY })
await ev(`localStorage.setItem('knowisle.lastVault', ${JSON.stringify(VAULT)})`)
await send('Page.reload', {})

/* ───────────────────── 输入合成 ───────────────────── */

const MOD = { alt: 1, ctrl: 2, meta: 4, shift: 8 }

const keyDown = (key, code, vk, modifiers = 0, text = '') =>
  send('Input.dispatchKeyEvent', {
    type: 'keyDown',
    key,
    code,
    windowsVirtualKeyCode: vk,
    nativeVirtualKeyCode: vk,
    modifiers,
    ...(text === '' ? {} : { text, unmodifiedText: text }),
  })
const keyUp = (key, code, vk, modifiers = 0) =>
  send('Input.dispatchKeyEvent', {
    type: 'keyUp',
    key,
    code,
    windowsVirtualKeyCode: vk,
    nativeVirtualKeyCode: vk,
    modifiers,
  })

const press = async (key, code, vk, modifiers = 0) => {
  await keyDown(key, code, vk, modifiers)
  await keyUp(key, code, vk, modifiers)
}

const CTRL_P = () => press('p', 'KeyP', 80, MOD.ctrl)
const ESC = () => press('Escape', 'Escape', 27)
const ENTER = () => press('Enter', 'Enter', 13)
const BACKSPACE = () => press('Backspace', 'Backspace', 8)

/** 往当前焦点里插一段文本。中文走这里：`dispatchKeyEvent` 合成不了汉字。 */
const insertText = async (text) => {
  await send('Input.insertText', { text })
  await sleep(12)
}

/* ───────────────────── 读取面板状态 ───────────────────── */

const panel = () =>
  ev(`(() => {
    const root = document.querySelector('.quick-open')
    if (root === null) return { open: false }
    const items = [...document.querySelectorAll('.quick-open__item')].map((el) => ({
      path: el.dataset.path,
      name: el.querySelector('.quick-open__name')?.textContent ?? '',
      dir: el.querySelector('.quick-open__dir')?.textContent ?? '',
      active: el.classList.contains('is-active'),
    }))
    const input = document.querySelector('.quick-open__input')
    return {
      open: true,
      value: input?.value ?? null,
      items,
      count: items.length,
      summary: document.querySelector('.quick-open__summary')?.textContent ?? '',
      activeIndex: items.findIndex((i) => i.active),
      activedescendant: input?.getAttribute('aria-activedescendant'),
      focus: document.activeElement?.className ?? '',
    }
  })()`)

const openPanel = async () => {
  if ((await panel()).open) return
  await CTRL_P()
  await sleep(160)
}

const closePanel = async () => {
  if (!(await panel()).open) return
  await ESC()
  await sleep(120)
}

/** 重新打开一次，拿到一个「空查询 + 焦点在 input」的干净状态。 */
const freshPanel = async () => {
  await closePanel()
  await openPanel()
}

const typeQuery = async (query) => {
  for (const ch of query) await insertText(ch)
  await sleep(90)
}

/* ───────────────────── 结果收集 ───────────────────── */

const results = {}
const fail = []
const skipped = []

const check = (name, ok, detail) => {
  results[name] = { ok, detail }
  if (!ok) fail.push(`${name}: ${JSON.stringify(detail)}`)
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}

const skip = (name, reason) => {
  results[name] = { ok: null, skipped: true, reason }
  skipped.push({ name, reason })
  console.log(`SKIP  ${name}  ${reason}`)
}

const stats = (xs) => {
  // 空样本必须显式返回 n=0，让判据 FAIL。旧版这里直接 `q(0.5).toFixed()`，
  // 空数组会抛 TypeError——脚本崩在「统计」这一步，看起来像脚本写错了，
  // 而不是「一条样本都没采到」。
  if (xs.length === 0) return { n: 0, p50: null, p95: null, max: null }
  const s = [...xs].sort((a, b) => a - b)
  const q = (p) => s[Math.min(s.length - 1, Math.floor((s.length - 1) * p))]
  return {
    n: s.length,
    p50: +q(0.5).toFixed(2),
    p95: +q(0.95).toFixed(2),
    max: +s[s.length - 1].toFixed(2),
  }
}

/* ───────────────────── 0. 库加载完成 ───────────────────── */

let loaded = null
for (let i = 0; i < 60; i += 1) {
  loaded = await ev(`(() => {
    const stats = document.querySelector('.stats')?.textContent ?? ''
    return { stats, rows: document.querySelectorAll('.tree-row').length }
  })()`)
  if (loaded.stats.includes('1000 篇')) break
  await sleep(200)
}
// 夹具是 1000 篇笔记 + 2 个坏文件（GBK / 超大），扫描器两个都数得进树，
// 所以状态栏是 1002。这里断言 1002 而不是 1000：`.stats` 报的是**树里有几个
// 文件**，不是"索引成功了几篇"。
check('0.vault', loaded.stats.includes('1002 篇'), loaded)

// 监听器基线：探针数的是 `window` 上**所有** `keydown` 监听器，而 App 现在
// 有四个快捷键、各自一个 effect（M2 第三刀加的 `Mod+N` / 侧栏）。所以
// 「必须等于 1」从那时起就没有依据了，第 8 节验的是**增长**而不是绝对值：
// 每个 effect 都带 cleanup，一次完整交互前后数量必须一模一样；漏了 cleanup
// 或者每次渲染都重新注册，它就会涨。
const baselineListeners = await ev(`window.__KI_KD__.length`)

/* ───────────────────── 1. 打开面板到列表出现 ───────────────────── */

await ev(`(() => {
  window.__KI_OPEN__ = []
  window.__KI_OPEN_T0__ = null
  const kd = (e) => {
    if ((e.ctrlKey || e.metaKey) && (e.key === 'p' || e.key === 'P')) {
      window.__KI_OPEN_T0__ = performance.now()
    }
  }
  kd.__kiTest = true
  window.addEventListener('keydown', kd, true)
  window.__KI_OPEN_MO__ = new MutationObserver(() => {
    if (window.__KI_OPEN_T0__ === null) return
    if (document.querySelector('.quick-open__item') === null) return
    window.__KI_OPEN__.push(performance.now() - window.__KI_OPEN_T0__)
    window.__KI_OPEN_T0__ = null
  })
  window.__KI_OPEN_MO__.observe(document.querySelector('.app'), { childList: true, subtree: true })
})()`)

for (let i = 0; i < 10; i += 1) {
  await CTRL_P()
  await sleep(220)
  await ESC()
  await sleep(220)
}
const openSamples = await ev(`window.__KI_OPEN__`)
const openStats = stats(openSamples)
check('1.open-latency', openStats.n === 10 && openStats.p95 <= 100 && openStats.max <= 100, openStats)

/* ───────────────────── 2. 空查询：前 50 篇按 path 排序 ───────────────────── */

await freshPanel()
const empty = await panel()
check('2.empty-count', empty.count === 50, { count: empty.count, summary: empty.summary })
check(
  '2.empty-order',
  empty.items.every((it, i) => it.path === EXPECTED_TOP50[i]),
  {
    first5: empty.items.slice(0, 5).map((i) => i.path),
    mismatch: empty.items.findIndex((it, i) => it.path !== EXPECTED_TOP50[i]),
  },
)
check('2.empty-hint', empty.summary === '没有打开历史，按路径排序', empty.summary)
check('2.focus-on-input', empty.focus.includes('quick-open__input'), empty.focus)
check(
  '2.activedescendant',
  empty.activedescendant === 'ki-quick-opt-0' && empty.activeIndex === 0,
  { activedescendant: empty.activedescendant, activeIndex: empty.activeIndex },
)

/* ───────────────────── 3. 模糊匹配质量（5 组） ───────────────────── */

const GROUPS = [
  { name: '3a.consecutive', query: 'readme', expect: 'docs/readme.md' },
  { name: '3b.segment-start', query: 'bar', expect: 'foo/bar.md' },
  { name: '3c.case', query: 'API', expect: 'alpha/API-Design.md' },
  { name: '3d.cjk', query: '知识', expect: '笔记/知识管理.md' },
  { name: '3e.path', query: 'foo/bar', expect: 'foo/bar.md' },
]

for (const group of GROUPS) {
  await freshPanel()
  await typeQuery(group.query)
  const state = await panel()
  check(group.name, state.items[0]?.path === group.expect, {
    query: group.query,
    got: state.items.slice(0, 3).map((i) => i.path),
    want: group.expect,
    hits: state.count,
  })
}

/* ───────────────────── 4. 100 次击键的 p95 ───────────────────── */

await freshPanel()
// 探针：keydown → 列表 DOM 真的变了（MutationObserver 微任务）。
//
// 不用 `document` 上的 `input` 事件做副判据：CDP 合成的文本插入不会
// 让 `input` 冒泡到 document（实测 0 条），拿它当分母只会得到空数组。
await ev(`(() => {
  window.__KI_PERF__ = []
  window.__KI_T0__ = null
  const kd = (e) => {
    if (e.key.length === 1) window.__KI_T0__ = performance.now()
  }
  kd.__kiTest = true
  window.addEventListener('keydown', kd, true)
  window.__KI_PERF_MO__ = new MutationObserver(() => {
    if (window.__KI_T0__ === null) return
    window.__KI_PERF__.push(performance.now() - window.__KI_T0__)
    window.__KI_T0__ = null
  })
  window.__KI_PERF_MO__.observe(document.querySelector('.quick-open'), {
    childList: true,
    subtree: true,
    characterData: true,
  })
})()`)

const armAtStart = await ev(`({
  panels: document.querySelectorAll('.quick-open').length,
  focus: document.activeElement?.className ?? '',
  value: document.querySelector('.quick-open__input')?.value ?? null,
})`)
console.log('  布防：', JSON.stringify(armAtStart))

// `a` → `al` → `alp`：每一步都真的缩小结果集（50 → 41 → 40），
// 所以每一次击键都会产生 DOM 变更。用 `n`/`o`/`t` 会踩到「`not` 与 `no`
// 的前 50 名完全相同」——第 2、3 次击键没有 DOM 变更，MutationObserver
// 不会触发，样本数悄悄少三分之一（实测 68/102），而 p95 里混进的是
// 「跨了整轮」的假样本。
const KEYS = [
  ['a', 'KeyA', 65],
  ['l', 'KeyL', 76],
  ['p', 'KeyP', 80],
]
const ROUNDS = 34
for (let r = 0; r < ROUNDS; r += 1) {
  for (const [k, code, vk] of KEYS) {
    await keyDown(k, code, vk, 0, k)
    await keyUp(k, code, vk)
    await sleep(16)
  }
  for (let i = 0; i < 3; i += 1) {
    await BACKSPACE()
    await sleep(16)
  }
}
await sleep(200)
const perfSamples = await ev(`window.__KI_PERF__`)
const endState = await ev(`({
  panels: document.querySelectorAll('.quick-open').length,
  focus: document.activeElement?.className ?? '',
  value: document.querySelector('.quick-open__input')?.value ?? null,
})`)
console.log('  收尾：', JSON.stringify({ samples: perfSamples.length, endState }))
const perf = stats(perfSamples)
check('4.keystroke-p95', perf.n >= ROUNDS * KEYS.length && perf.p95 <= 16, {
  ...perf,
  expectedSamples: ROUNDS * KEYS.length,
})
await ev(`window.__KI_PERF_MO__.disconnect()`)

/* ───────────────────── 5. Enter 打开 + 上一篇已 flush ───────────────────── */

await closePanel()

// A：用面板打开（打开路径与树点击是同一条，见 App.openNote）。
await openPanel()
await typeQuery('note-0000')
const aHit = (await panel()).items[0]
await ENTER()
await sleep(700)

const aOpened = await ev(`document.querySelector('.breadcrumb')?.textContent ?? ''`)
check('5a.open-a', aOpened.includes('note-0000.md'), { hit: aHit?.path, breadcrumb: aOpened })

// 在 A 的末尾敲一个唯一标记，**不等** 300ms 合并窗口，直接切走。
const MARKER = `KI-FLUSH-${Date.now()}`
await ev(`document.querySelector('.cm-content').focus()`)
await press('End', 'End', 35, MOD.ctrl)
await insertText(MARKER)
await sleep(60)

await openPanel()
await typeQuery('foo/bar')
const bHit = (await panel()).items[0]
await ENTER()
await sleep(800)

const bOpened = await ev(`(() => ({
  breadcrumb: document.querySelector('.breadcrumb')?.textContent ?? '',
  panel: document.querySelectorAll('.quick-open').length,
}))()`)
check('5b.open-b', bHit?.path === NOTE_B && bOpened.breadcrumb.includes('bar.md'), {
  hit: bHit?.path,
  breadcrumb: bOpened.breadcrumb,
})
check('5c.panel-closed', bOpened.panel === 0, bOpened)

let aDisk = ''
for (let i = 0; i < 20; i += 1) {
  aDisk = fs.readFileSync(NOTE_A, 'utf8')
  if (aDisk.includes(MARKER)) break
  await sleep(100)
}
check('5d.previous-flushed', aDisk.includes(MARKER), {
  marker: MARKER,
  tail: JSON.stringify(aDisk.slice(-32)),
})

/* ───────────────────── 6. Esc：焦点回编辑器且光标不动 ───────────────────── */

// B 只有 7 行，CM6 会把整篇都渲染出来——于是「行号 + 行内偏移」可以直接
// 换算成绝对偏移，等价于 `selection.main.head`。
const CARET = `(() => {
  const sel = window.getSelection()
  if (sel === null || sel.rangeCount === 0) return null
  const node = sel.anchorNode
  const el = node === null ? null : node.nodeType === 3 ? node.parentElement : node
  const line = el?.closest('.cm-line') ?? null
  if (line === null) return null
  const lines = [...document.querySelectorAll('.cm-line')]
  const index = lines.indexOf(line)
  let before = 0
  for (let i = 0; i < index; i += 1) before += lines[i].textContent.length + 1
  const range = document.createRange()
  range.setStart(line, 0)
  range.setEnd(node, sel.anchorOffset)
  return {
    renderedLines: lines.length,
    lineIndex: index,
    offsetInLine: range.toString().length,
    head: before + range.toString().length,
    focus: document.activeElement?.className ?? '',
  }
})()`

const lineCount = await ev(`document.querySelectorAll('.cm-line').length`)
const clickPoint = await ev(`(() => {
  const lines = [...document.querySelectorAll('.cm-line')]
  const line = lines[3] ?? lines[0]
  line.scrollIntoView({ block: 'center' })
  const r = line.getBoundingClientRect()
  return { x: r.left + 40, y: r.top + r.height / 2 }
})()`)
await send('Input.dispatchMouseEvent', { type: 'mousePressed', x: clickPoint.x, y: clickPoint.y, button: 'left', clickCount: 1 })
await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: clickPoint.x, y: clickPoint.y, button: 'left', clickCount: 1 })
await sleep(200)

const before = await ev(CARET)
await openPanel()
const whileOpen = await panel()
await ESC()
await sleep(220)
const after = await ev(CARET)

check('6a.esc-closes', whileOpen.open === true && (await panel()).open === false, {
  whileOpen: whileOpen.open,
})
check('6b.focus-editor', after !== null && after.focus.includes('cm-content'), after?.focus)
check(
  '6c.caret-unchanged',
  before !== null &&
    after !== null &&
    before.head === after.head &&
    before.lineIndex === after.lineIndex &&
    before.offsetInLine === after.offsetInLine,
  { before: before?.head, after: after?.head, lineCount, renderedLines: after?.renderedLines },
)

/* ───────────────────── 7. IME 组字期间 Enter 不打开 ───────────────────── */

await openPanel()
await typeQuery('foo/bar')
const imeBefore = await panel()

// 组字开始。随后派发的 Enter **故意**带 `isComposing: false`——
// 这样只有我们自己记的 composingRef 能拦住它。若实现只查 isComposing，
// 这一条会立刻变红。
await ev(`(() => {
  const input = document.querySelector('.quick-open__input')
  input.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: 'ba' }))
  input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', bubbles: true, cancelable: true, isComposing: false }))
})()`)
await sleep(250)
const imeDuring = await panel()
check('7a.ime-ref-blocks', imeDuring.open === true && imeDuring.items[0]?.path === NOTE_B, {
  open: imeDuring.open,
  hit: imeDuring.items[0]?.path,
  hitsBefore: imeBefore.count,
})

// 组字结束 → composingRef 归零。此时再派发一个 `isComposing: true` 的 Enter，
// 只有 `isComposing` 这一条能拦住它——单独验证第二条防线。
await ev(`(() => {
  const input = document.querySelector('.quick-open__input')
  input.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: 'ba' }))
  input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', code: 'Enter', bubbles: true, cancelable: true, isComposing: true }))
})()`)
await sleep(200)
check('7b.iscomposing-blocks', (await panel()).open === true, { open: (await panel()).open })

// 真正的 Enter 必须正常打开。
await ENTER()
await sleep(700)
const afterIme = await ev(`(() => ({
  open: document.querySelectorAll('.quick-open').length,
  breadcrumb: document.querySelector('.breadcrumb')?.textContent ?? '',
}))()`)
check('7c.enter-after-ime-opens', afterIme.open === 0 && afterIme.breadcrumb.includes('bar.md'), afterIme)

/* ───────────────────── 8. StrictMode：监听器不泄漏 ───────────────────── */

const listeners = await ev(`window.__KI_KD__.length`)
check('8a.single-listener', listeners > 0 && listeners === baselineListeners, {
  netListeners: listeners,
  baseline: baselineListeners,
})

await ev(`window.__KI_KD_INVOKED__ = 0`)
await CTRL_P()
await sleep(200)
// `invocations` 是**所有** window keydown 监听器的调用次数，所以它等于
// 监听器数而不是 1——`=== 1` 是「App 只有一个快捷键」时代的断言。这里只
// 要求它 > 0（按键真的被处理了），"只开一次"交给浮层数来证。
const invocations = await ev(`window.__KI_KD_INVOKED__`)
const overlays = await ev(`document.querySelectorAll('.quick-open').length`)
check('8b.one-invocation', invocations > 0 && overlays === 1, { invocations, overlays })
const focusNow = await ev(`document.activeElement?.className ?? ''`)
check('8c.one-overlay', overlays === 1 && focusNow.includes('quick-open__input'), {
  overlays,
  focus: focusNow,
})
await ESC()
await sleep(150)

// 点一次 Enter 只打开一次：再走一遍完整路径，确认浮层只关一次、笔记只换一次。
await openPanel()
await typeQuery('foo/bar')
await ENTER()
await sleep(600)
const once = await ev(`(() => ({
  open: document.querySelectorAll('.quick-open').length,
  breadcrumb: document.querySelector('.breadcrumb')?.textContent ?? '',
  listeners: window.__KI_KD__.length,
}))()`)
check(
  '8d.enter-opens-once',
  once.open === 0 && once.breadcrumb.includes('bar.md') && once.listeners === baselineListeners,
  { ...once, baseline: baselineListeners },
)

/* ───────────────────── 汇总 ───────────────────── */

const passed = Object.values(results).filter((r) => r.ok === true).length
console.log('\n' + JSON.stringify({ passed, skipped: skipped.length, failed: fail.length }, null, 2))
console.log(`\n通过 ${passed} · 跳过 ${skipped.length} · 失败 ${fail.length}`)
if (skipped.length > 0) {
  console.error(`⚠ ${skipped.length} 项**没有执行**：本次验收不完整，不能当全绿看。`)
}
if (fail.length > 0) {
  console.error(fail.join('\n'))
  process.exitCode = 1
} else if (skipped.length > 0) {
  process.exitCode = 2
}
ws.close()
