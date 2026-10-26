// M6 第二刀 · 后台索引与「可见的就绪状态」端到端验收。真实 WebView2 + CDP。
//
// # 它证明什么
//
// `vault_scan` 只等树，索引在后台建。于是搜索可能不全——这件事**必须可见**，
// 否则「刚建的文件搜不到」就是一句无从解释的空白（B-09）。本脚本验四件事：
//
//   1. 树先出来：`.tree-row` 出现时，索引状态还是 `building`；
//   2. 状态跟着搜索走：IPC 返回的 `indexState` 是 `building` 且 `done < total`；
//   3. 文案真的出现：`.search__indexing` 的文本是「索引正在建立（N/M），
//      以下结果可能不全」——**断言 DOM，不只断言 JSON**；
//   4. 就绪之后：`indexState` 变 `ready`、`indexedFiles` 等于篇数、那一行文案消失。
//
// # 前置（缺一条都不算数）
//
//   1. `pnpm dev`（vite 在 127.0.0.1:1420，且已用 `curl --noproxy '*'` 核对过）；
//   2. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16）；
//   3. 运行期间源码冻结。
//
// # 为什么夹具是 3000 篇
//
// 这条用例需要一个**够宽的**「正在建」窗口：1000 篇在 release 下 1 秒就建完，
// 脚本还没连上就结束了。3000 篇在 debug 版下是几秒，足够完成一次搜索并读回文案。
//
// 跑法：`node scripts/e2e/cdp-index-status.mjs`
import fs from 'node:fs'
import path from 'node:path'

import { APP_ORIGIN, CDP_PORT as PORT, INDEX_VAULT as VAULT, OUT_DIR, sleep } from './env.mjs'
import { wipeVault } from './fixtures.mjs'

/** 夹具篇数。见文件头「为什么是 3000」。 */
const FILES = 3000
/** 每篇正文里都有这个词，所以它一定命中全部笔记。 */
const QUERY = '本地优先'
const NOTICE = /^索引正在建立（(\d+)\/(\d+)），以下结果可能不全$/

/* ───────────────────────────── 夹具 ───────────────────────────── */

/**
 * 幂等：清空同名目录再重建（保留 `.knowisle/`，见 `wipeVault` 的说明）。
 *
 * 放系统临时目录（`env.mjs` 的 `INDEX_VAULT`），不放仓库的 `.tmp/`：
 * 那里删一个文件 390ms，3000 个要十几分钟。
 *
 * 保留索引不影响「够宽的正在建窗口」：每篇都会被重写，mtime 全变，
 * 增量同步要把 3000 篇重新读一遍再分批提交，debug 版下是几秒。
 */
function prepareVault() {
  const root = VAULT
  wipeVault(root)
  fs.mkdirSync(root, { recursive: true })
  for (let i = 0; i < FILES; i += 1) {
    const body = `# 笔记 ${String(i).padStart(5, '0')}

本地优先的知识库把正文留在磁盘上，索引只是派生数据。
这一篇用来给「后台索引」的验收撑出一个足够宽的窗口，编号 ${String(i).padStart(5, '0')}。
`
    fs.writeFileSync(path.join(root, `n${String(i).padStart(5, '0')}.md`), body, 'utf8')
  }
  return root
}

const fixture = prepareVault()
console.log(`夹具：${FILES} 篇 → ${fixture}`)

/* ───────────────────────────── CDP ───────────────────────────── */

const targets = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
// 只认 URL 落在 dev 地址上的 target：`/json` 里可能同时有 `about:blank` 的僵尸，
// `find(t => t.type === 'page')` 抓到前者会永远读不到地址（TOOLING）。
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

const search = (query) => invoke('vault_search', { root: VAULT, query, refreshPath: null })

/** 轮询到表达式为真。返回耗时（毫秒），超时抛错。 */
async function waitFor(expression, label, timeoutMs = 120_000, interval = 30) {
  const started = Date.now()
  while (Date.now() - started < timeoutMs) {
    if (await ev(expression)) return Date.now() - started
    await sleep(interval)
  }
  throw new Error(`等待超时：${label}（${timeoutMs}ms）`)
}

/* ───────────────────────────── 结果收集 ───────────────────────────── */

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  console.log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
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

/** Ctrl+Shift+F（modifiers: 2 = Ctrl，8 = Shift）。 */
const openSearch = async () => {
  if (await ev(`document.querySelector('.search__input') !== null`)) return
  await pressKey('F', 'KeyF', 70, 2 | 8)
  await waitFor(`document.querySelector('.search__input') !== null`, '搜索浮层出现', 5_000, 20)
}

const closeSearch = async () => {
  if (await ev(`document.querySelector('.search__input') === null`)) return
  await pressKey('Escape', 'Escape', 27)
  await sleep(120)
}

/**
 * 往搜索框里填一个词。
 *
 * 不用 `Input.insertText`：它要求**真焦点**，而无 OS 焦点的桌面上
 * `focus()` 落不住，字符一个都进不去且帧上完全看不出来（TOOLING）。
 * 用原生 setter + `input` 事件走的是 React 的受控更新，等价于用户输入，
 * 但与时序无关——这条用例要验的是文案，不是键盘。
 */
const fillQuery = async (text) => {
  await ev(`(() => {
    const input = document.querySelector('.search__input')
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set
    setter.call(input, ${JSON.stringify(text)})
    input.dispatchEvent(new Event('input', { bubbles: true }))
  })()`)
}

const noticeText = () => ev(`document.querySelector('.search__indexing')?.textContent ?? null`)

/* ───────────────────────── 0. 加载库（树先出来） ───────────────────────── */

await ev(`localStorage.setItem('knowisle.onboarded', '1')`)
await ev(`localStorage.setItem('knowisle.lastVault', ${JSON.stringify(VAULT)})`)

const reloadedAt = Date.now()
await send('Page.reload', {})

// `.stats` 是树渲染完成之后才有的（它显示「N 篇」）。等它就是等树。
const treeMs = await waitFor(
  `document.querySelector('.stats')?.textContent?.includes(${JSON.stringify(`${FILES} 篇`)}) === true`,
  '树渲染完成',
  60_000,
  20,
)
console.log(`树出现：${Date.now() - reloadedAt}ms（其中轮询到 .stats 用了 ${treeMs}ms）`)

/* ─────────────────── 1. 树出来时索引还在建（IPC 事实） ─────────────────── */

const first = await search(QUERY)
const firstState = first.indexState
check(
  '1.tree-before-index',
  firstState.state === 'building' && firstState.done < firstState.total,
  { at: `${Date.now() - reloadedAt}ms`, indexState: firstState, indexedFiles: first.indexedFiles },
)

/* ─────────────────── 2. 文案真的出现（DOM 断言） ─────────────────── */

await openSearch()
await fillQuery(QUERY)

// 等文案出现；它跟着 `vault_search` 的返回渲染，所以最多一次 IPC 往返。
await waitFor(`document.querySelector('.search__indexing') !== null`, '索引状态文案出现', 20_000, 20)
const text = await noticeText()
const matched = NOTICE.exec(text ?? '')
check('2.notice-visible', matched !== null, { text })
check(
  '2.notice-progress',
  matched !== null && Number(matched[1]) < Number(matched[2]),
  { done: matched?.[1], total: matched?.[2] },
)

// 同一时刻的 IPC 事实必须与文案一致：文案不是写死的，它来自 `indexState`。
// `done` 只会往前走（文案是上一次 IPC 的值），`total` 建完之前是固定的。
const during = await search(QUERY)
check(
  '2.notice-matches-ipc',
  during.indexState.state === 'building' &&
    matched !== null &&
    during.indexState.total === Number(matched[2]) &&
    during.indexState.done >= Number(matched[1]),
  { indexState: during.indexState, notice: { done: matched?.[1], total: matched?.[2] } },
)

/* ─────────────────── 3. 就绪：状态与计数 ─────────────────── */

const readyAt = Date.now()
let ready = null
for (;;) {
  const result = await search(QUERY)
  if (result.indexState.state === 'ready') {
    ready = result
    break
  }
  if (Date.now() - readyAt > 180_000) throw new Error('索引没能在 180 秒内就绪')
  await sleep(200)
}
check('3.ready-state', ready.indexState.state === 'ready', {
  afterMs: Date.now() - reloadedAt,
  indexState: ready.indexState,
})
check('3.ready-count', ready.indexedFiles === FILES, {
  indexedFiles: ready.indexedFiles,
  want: FILES,
  skipped: ready.skippedFiles,
})
check('3.ready-hits', ready.total > 0 && ready.hits.length > 0, {
  total: ready.total,
  hits: ready.hits.length,
})
// 顺序：树先出来，索引后建完。这一条是 GUI 版的验收 1——
// bench 里量的是 `vault_scan` 的返回时间，这里量的是"用户真的看见树"的时刻。
check('3.tree-was-early', Date.now() - reloadedAt > treeMs, {
  treeMs,
  readyMs: Date.now() - reloadedAt,
})

/* ─────────────────── 4. 文案消失 ─────────────────── */

await closeSearch()
await openSearch()
await fillQuery(QUERY)
// 等一次新的 IPC 落地：结果条数出现即可（文案此时应当已经不在了）。
await waitFor(
  `document.querySelectorAll('.search__item').length > 0`,
  '就绪后的命中列表出现',
  20_000,
  20,
)
const after = await noticeText()
check('4.notice-gone', after === null, { text: after })

const summary = await ev(`document.querySelector('.search__summary')?.textContent ?? ''`)
check('4.summary', summary.includes(`索引 ${FILES} 篇`), { summary })

/* ───────────────────────────── 汇总 ───────────────────────────── */

const passed = results.filter((r) => r.ok === true).length
const failed = results.filter((r) => r.ok === false).length
console.log(`\n汇总：${passed} 通过 / ${failed} 失败`)

fs.mkdirSync(OUT_DIR, { recursive: true })
fs.writeFileSync(
  path.join(OUT_DIR, 'index-status-acceptance.json'),
  JSON.stringify({ fixture: VAULT, files: FILES, results }, null, 2),
)

if (failed > 0) {
  console.error(results.filter((r) => !r.ok).map((r) => `${r.name}: ${JSON.stringify(r.detail)}`).join('\n'))
  process.exitCode = 1
}
ws.close()
