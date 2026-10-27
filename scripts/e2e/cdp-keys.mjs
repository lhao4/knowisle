// M2 第七刀 · 剩余 4 个快捷键（新建 / 侧栏 / 加粗 / 斜体）的端到端验收。
// 真实 WebView2 + CDP。覆盖验收 1–10（11 是命令行检查，不在本脚本里）。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`：vite 在 127.0.0.1:1420（脚本会取 /src/App.tsx 核对转译结果，B-18）
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16：调试槽位是进程级静态）
//      或 `pnpm e2e:keys --restart` 让脚本自己杀掉并重启
//   4. 运行期间源码冻结
//
// 结构：
//   A 冷启动、无库              → 验收 1a
//   B 打开夹具库                → 验收 1b / 1c / 2 / 3 / 4 / 5
//   C 标记（加粗 / 斜体）        → 验收 6 / 7 / 8 / 9
//   D 重启进程（B-16）           → 验收 10（含 Mod+S 落盘）
//
// 夹具：`os.tmpdir()/knowisle-e2e-keys`，每次重建（B-32）。之前它靠
// `.tmp/vault-m2-7` 里手搓的 `日记/2026`、`标记.md`、`保存.md`。
//
// 跑法：`pnpm e2e:keys`（或加 `--restart`）
// 退出码只有三种（B-19）：0 全跑全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'

import { openApp, killApp, MOD } from './cdp.mjs'
import { CDP_PORT, KEYS_VAULT as VAULT, OUT_DIR, sleep } from './env.mjs'
import { resetVault } from './fixtures.mjs'

const SHOT = OUT_DIR

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  console.log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}
/**
 * 显式跳过：进 `results`（`ok: null`）、进汇总、退出码 2。
 *
 * 不能用 `if (条件) check(...)` 代替——那样跳过项根本不进 `results`，
 * 脚本照样 exit 0、打印「全部通过」，读者分不清「验过了」和「压根没跑」（B-19）。
 */
const skip = (name, reason) => {
  results.push({ name, ok: null, detail: reason })
  console.log(`SKIP  ${name}  ${reason}`)
}

/* ───────────────────────── 夹具 ───────────────────────── */

/**
 * `标记.md` 必须是 `abc`（C0 的判据），`保存.md` 是 Mod+S 的落点；
 * `日记/2026` 用来验「折叠侧栏不卸载树节点」。每次全量重写，上一轮的
 * `快捷根*.md` 与 `快捷子*.md` 不会留在库里改变「只有一个」这类计数（B-32）。
 */
const buildFixture = () =>
  resetVault(
    VAULT,
    {
      '标记.md': 'abc',
      '保存.md': '初始',
      '日记/日记一.md': '# 日记一\n\n正文。\n',
      '日记/2026/一月.md': '# 一月\n\n正文。\n',
    },
    { clearTrash: true },
  )

/* ───────────────────────── 会话 ───────────────────────── */

const consoleErrors = []
let app
let send
let ev
let waitFor
let press
let clickAt
let insertText

const bind = (session) => {
  app = session
  send = session.send
  ev = session.ev
  waitFor = session.waitFor
  press = session.press
  clickAt = session.clickAt
  insertText = session.insertText
}

// A 段：冷启动、**没有库**。openApp 会清掉 lastVault 并等到欢迎页。
bind(await openApp({ vault: null, ready: '.welcome', prepare: buildFixture, errors: consoleErrors }))

/* ───────────────────────── 输入合成 ───────────────────────── */

const ESC = () => press('Escape', 'Escape', 27)
const ENTER = () => press('Enter', 'Enter', 13)
const CTRL_N = () => press('n', 'KeyN', 78, MOD.ctrl)
const CTRL_B = () => press('b', 'KeyB', 66, MOD.ctrl)
const CTRL_I = () => press('i', 'KeyI', 73, MOD.ctrl)
const CTRL_Z = () => press('z', 'KeyZ', 90, MOD.ctrl)
const CTRL_A = () => press('a', 'KeyA', 65, MOD.ctrl)
const CTRL_S = () => press('s', 'KeyS', 83, MOD.ctrl)
const CTRL_P = () => press('p', 'KeyP', 80, MOD.ctrl)
const CTRL_SHIFT_F = () => press('f', 'KeyF', 70, MOD.ctrl | MOD.shift)
// 反斜杠的 vk 是 220（VK_OEM_5）
const CTRL_BACKSLASH = () => press('\\', 'Backslash', 220, MOD.ctrl)
const CTRL_HOME = () => press('Home', 'Home', 36, MOD.ctrl)
const CTRL_END = () => press('End', 'End', 35, MOD.ctrl)
const CTRL_SHIFT_END = () => press('End', 'End', 35, MOD.ctrl | MOD.shift)
const RIGHT = (n = 1) => press('ArrowRight', 'ArrowRight', 39).then(() => (n > 1 ? RIGHT(n - 1) : null))
const SHIFT_RIGHT = (n = 1) =>
  press('ArrowRight', 'ArrowRight', 39, MOD.shift).then(() => (n > 1 ? SHIFT_RIGHT(n - 1) : null))

const rowRect = (title) =>
  ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    if (!el) return null
    el.scrollIntoView({ block: 'center' })
    const r = el.getBoundingClientRect()
    return { x: r.left + 40, y: r.top + r.height / 2 }
  })()`)

/**
 * 打开一篇笔记（走树点击，和用户一样）。
 *
 * 最多点三次，每次重新取坐标：新建一篇之后应用还在重扫，树会在我们算出坐标
 * 与真正点击之间多出一行，那一次点击就落到邻居身上——1d 就这样红过一次，
 * 而红的时候面包屑还是上一篇，看起来像"打开笔记坏了"。重试**必须重新取坐标**，
 * 否则三次点的是同一个错位置。
 *
 * 三次都不行仍然返回 false，由调用方断言——这不是"重试到绿"，是给
 * 「树正在变」这个真实窗口留出余量。
 */
const openNote = async (title) => {
  const want = `document.querySelector('.breadcrumb')?.textContent?.includes(${JSON.stringify(title)}) === true`
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const r = await rowRect(title)
    if (r === null) throw new Error(`树里找不到 ${title}`)
    await clickAt(r.x, r.y)
    if (await waitFor(want, 4000)) return true
  }
  return false
}

/* ───────────────────── 编辑器状态（CM6 句柄） ───────────────────── */
// `.cm-content` 上挂着 CM6 的 `cmTile`，`cmTile.root.view` 就是 EditorView。
// 只读它的 state——不 dispatch、不改 DOM，所以它不会影响被测行为。

const STATE_JS = `(() => {
  const el = document.querySelector('.cm-content')
  const v = el && el.cmTile && el.cmTile.root && el.cmTile.root.view
  if (!v) return null
  const s = v.state.selection.main
  return { doc: v.state.doc.toString(), anchor: s.anchor, head: s.head, from: s.from, to: s.to }
})()`
const state = () => ev(STATE_JS)

const focusEditor = async () => {
  await ev(`document.querySelector('.cm-content')?.focus()`)
  return waitFor(`document.activeElement?.classList.contains('cm-content') === true`, 2000)
}

/** 把正文换成 `text`（全选 → 输入），光标落在末尾。 */
const setDoc = async (text) => {
  if (!(await focusEditor())) throw new Error('编辑器没有拿到焦点')
  await CTRL_A()
  await insertText(text)
  await sleep(80)
}

const measure = () =>
  ev(`(() => {
    const sb = document.querySelector('.sidebar')
    const cm = document.querySelector('.cm-content')
    return {
      display: getComputedStyle(sb).display,
      sidebar: sb.getBoundingClientRect().width,
      cm: cm ? cm.getBoundingClientRect().width : null,
      stored: localStorage.getItem('knowisle.sidebar'),
      expanded: document.querySelector('.icon-btn')?.getAttribute('aria-expanded') ?? null,
    }
  })()`)

const shot = async (name) => {
  const res = await send('Page.captureScreenshot', { format: 'png' })
  const file = path.join(SHOT, `${name}.png`)
  fs.writeFileSync(file, Buffer.from(res.result.data, 'base64'))
  return file
}

/* ═══════════════════════ A. 冷启动 · 无库 ═══════════════════════ */

// openApp 已经杀掉旧进程、起了新进程，并清掉 lastVault 等到欢迎页。
// 上一轮的 localStorage 会污染下一轮（BACKLOG）：侧栏状态也显式复位。
await ev(`localStorage.setItem('knowisle.sidebar', '1')`)
await app.reload()
if (!(await waitFor(`document.querySelector('.welcome') !== null`, 8000))) {
  throw new Error('欢迎页没有出现（vite 挂了？）')
}

const errorsBefore1a = consoleErrors.length
await CTRL_N()
await sleep(300)
const noVault = {
  prompts: await ev(`document.querySelectorAll('.prompt').length`),
  welcome: await ev(`document.querySelector('.welcome') !== null`),
  targets: (await (await fetch(`http://127.0.0.1:${CDP_PORT}/json`)).json()).filter(
    (t) => t.type === 'page',
  ).length,
}
check(
  '1a 无库时 Mod+N：不开浮层、不弹新窗口、无 console error',
  noVault.prompts === 0 && noVault.welcome === true && noVault.targets === 1 &&
    consoleErrors.length === errorsBefore1a,
  noVault,
)

/* ═══════════════════════ B. 打开夹具库 ═══════════════════════ */

await ev(`localStorage.setItem('knowisle.lastVault', ${JSON.stringify(VAULT)})`)
await app.reload()
if (!(await waitFor(`document.querySelectorAll('.tree-row').length > 0`, 8000))) {
  throw new Error('夹具库没有被扫描出来')
}
await sleep(300)

/* ── 1b / 2. 未选中笔记 → 新建落在根目录；浮层已开时再按不叠层 ── */

const activeBefore = await ev(`document.querySelector('.breadcrumb') === null`)
await CTRL_N()
const promptOpened = await waitFor(`document.querySelector('.prompt') !== null`, 3000)
const promptInfo = {
  title: await ev(`document.querySelector('.prompt__title')?.textContent ?? null`),
  input: await ev(`document.querySelector('.prompt__input') !== null`),
}

// 验收 2：浮层已开时再按一次 Mod+N
await CTRL_N()
await sleep(250)
const promptCount = await ev(`document.querySelectorAll('.prompt').length`)

check(
  '1b 未选中笔记时 Mod+N → 浮层「新建笔记」',
  activeBefore === true && promptOpened === true && promptInfo.title === '新建笔记' &&
    promptInfo.input === true,
  { activeBefore, promptOpened, ...promptInfo },
)
check('2 浮层已开时再按 Mod+N → 屏幕上仍只有 1 个 .prompt', promptCount === 1, { promptCount })

// 真正提交一次：目标目录对不对，只有落盘能证明。
await insertText('快捷根')
await ENTER()
const createdRoot = []
for (let i = 0; i < 40; i += 1) {
  const hit = fs.readdirSync(VAULT).filter((f) => f.startsWith('快捷根'))
  if (hit.length > 0) {
    createdRoot.push(...hit)
    break
  }
  await sleep(100)
}
check(
  '1c 未选中笔记时新建 → 文件落在 Vault 根目录',
  createdRoot.length === 1 && createdRoot[0].endsWith('.md'),
  { createdRoot, cwd: fs.readdirSync(VAULT) },
)

/* ── 1d. 选中 日记/日记一.md 之后，目标目录跟着走 ── */

// 先等新建那一行真的进了树：`1c` 只证明了磁盘上有文件，而树的重扫是异步的。
// 不等就取坐标，点击可能落在"树刚好长高了一行"的邻居身上。
await waitFor(
  `[...document.querySelectorAll('.tree-row')].some((el) => el.title === '快捷根.md')`,
  4000,
)
const openedDiary = await openNote('日记/日记一.md')
await sleep(200)
await CTRL_N()
await waitFor(`document.querySelector('.prompt') !== null`, 3000)
await insertText('快捷子')
await ENTER()
const createdDiary = []
for (let i = 0; i < 40; i += 1) {
  const hit = fs.readdirSync(path.join(VAULT, '日记')).filter((f) => f.startsWith('快捷子'))
  if (hit.length > 0) {
    createdDiary.push(...hit)
    break
  }
  await sleep(100)
}
check(
  '1d 选中 日记/日记一.md 后新建 → 文件落在 日记/',
  openedDiary === true && createdDiary.length === 1 && createdDiary[0].endsWith('.md'),
  { openedDiary, createdDiary, diaryDir: fs.readdirSync(path.join(VAULT, '日记')) },
)

/* ── 3. 折叠：display:none + 宽度 0 + 正文吃掉 300px ── */

// 新建完会切到新笔记，先等编辑器挂上
if (!(await waitFor(`document.querySelector('.cm-content') !== null`, 4000))) {
  throw new Error('新建之后编辑器没有出现')
}
await sleep(200)

const expandedState = await measure()
await CTRL_BACKSLASH()
await sleep(200)
const collapsedState = await measure()
await CTRL_BACKSLASH()
await sleep(200)
const restoredState = await measure()

const grew = collapsedState.cm - expandedState.cm
const back = Math.abs(restoredState.cm - expandedState.cm)
check(
  '3 折叠：sidebar display=none / 宽 0；正文 +300±2；再按恢复（±2）',
  collapsedState.display === 'none' &&
    collapsedState.sidebar === 0 &&
    Math.abs(grew - 300) <= 2 &&
    back <= 2 &&
    restoredState.display !== 'none' &&
    Math.abs(restoredState.sidebar - 300) <= 0.5,
  { expanded: expandedState, collapsed: collapsedState, restored: restoredState, grew, back },
)
check(
  '3b 顶栏按钮的 aria-expanded 跟着状态走',
  expandedState.expanded === 'true' &&
    collapsedState.expanded === 'false' &&
    restoredState.expanded === 'true',
  [expandedState.expanded, collapsedState.expanded, restoredState.expanded],
)

const entry = await ev(`(() => {
  const btn = document.querySelector('.icon-btn')
  if (!btn) return null
  const actions = btn.parentElement
  const kids = [...actions.children]
  return {
    title: btn.title,
    label: btn.getAttribute('aria-label'),
    controls: btn.getAttribute('aria-controls'),
    beforeTheme: kids.indexOf(btn) < kids.findIndex((e) => e.classList.contains('theme-toggle')),
    sidebarId: document.getElementById(btn.getAttribute('aria-controls'))?.className ?? null,
  }
})()`)
check(
  '3c 顶栏可见入口：在主题按钮左边、title 带键位、aria-controls 指向侧栏',
  entry !== null &&
    entry.title.includes('Mod+\\') &&
    entry.beforeTheme === true &&
    entry.controls === 'ki-sidebar' &&
    entry.sidebarId === 'sidebar',
  entry,
)

/* ── 4. 折叠不丢展开态（不条件渲染） ── */

// `日记/2026` 在 depth 1，初值是折叠的 → 点开它，再折叠/展开侧栏。
const dirRect = await rowRect('日记/2026')
if (dirRect === null) throw new Error('树里找不到 日记/2026')
await clickAt(dirRect.x, dirRect.y)
await sleep(150)
const beforeFold = await ev(`(() => {
  const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === '日记/2026')
  window.__ki2026 = el
  el.dataset.kiProbe = 'PROBE-2026'
  return el.getAttribute('aria-expanded')
})()`)

await CTRL_BACKSLASH()
await sleep(180)
// display:none 的元素仍在 DOM 里、属性照样读得到——这本身就是"没被卸载"的证据。
const whileHidden = await ev(`window.__ki2026.getAttribute('aria-expanded')`)
await CTRL_BACKSLASH()
await sleep(180)
const afterFold = await ev(`(() => {
  const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === '日记/2026')
  return { same: el === window.__ki2026, expanded: el.getAttribute('aria-expanded'), probe: el.dataset.kiProbe ?? null }
})()`)
check(
  '4 折叠一次再展开：目录仍是展开态，且是同一个 DOM 节点（没被卸载）',
  beforeFold === 'true' && whileHidden === 'true' && afterFold.same === true &&
    afterFold.expanded === 'true' && afterFold.probe === 'PROBE-2026',
  { beforeFold, whileHidden, afterFold },
)

/* ── 5. 持久化 + 首帧就是 0 宽 ── */

// 首帧探针：`.workspace` 第一次出现的那一帧，量侧栏宽度。
await send('Page.addScriptToEvaluateOnNewDocument', {
  source: `
    window.__kiFrame = { done: false }
    ;(function tick() {
      if (window.__kiFrame.done) return
      if (document.querySelector('.workspace') !== null) {
        var sb = document.querySelector('.sidebar')
        var app = document.querySelector('.app')
        window.__kiFrame = {
          done: true,
          display: sb ? getComputedStyle(sb).display : null,
          width: sb ? sb.getBoundingClientRect().width : -1,
          cls: app ? app.className : null,
          rows: document.querySelectorAll('.tree-row').length,
          state: document.readyState,
        }
        return
      }
      requestAnimationFrame(tick)
    })()
  `,
})

// 当前是展开态（验收 4 结束时又展开了）。折叠 → reload。
await CTRL_BACKSLASH()
await sleep(200)
const storedBefore = await ev(`localStorage.getItem('knowisle.sidebar')`)
await app.reload()
await waitFor(`document.querySelectorAll('.tree-row').length > 0`, 8000)
await sleep(250)
const afterReload = await measure()
const frame = await ev(`window.__kiFrame`)
check(
  '5 折叠后 reload：仍是折叠（持久化）',
  storedBefore === '0' && afterReload.display === 'none' && afterReload.sidebar === 0,
  { storedBefore, afterReload },
)
// 注意 `rows` 此时通常是 0：首帧只是外壳，库是异步扫出来的。这里要证明的是
// 「aside 已经存在、且第一帧就是隐藏的」，而不是「树已经填好了」。
check(
  '5b 首帧探针：.workspace 第一次出现的那一帧，侧栏宽 0（不是先 300 再变 0）',
  frame?.done === true && frame.display === 'none' && frame.width === 0 &&
    String(frame.cls).includes('app--sidebar-collapsed'),
  frame,
)

// 复位成展开，后面的标记测试与截图都在展开态下做。
await CTRL_BACKSLASH()
await sleep(200)

/* ═══════════════════════ C. 加粗 / 斜体 ═══════════════════════ */

if (!(await openNote('标记.md'))) throw new Error('打不开 标记.md')
await sleep(300)
const initial = await state()
check('C0 夹具就位：标记.md 正文是 abc', initial?.doc === 'abc', initial)

/* ── 6. 空选区 ── */

await setDoc('a\n\nb')
await CTRL_HOME()
await press('ArrowDown', 'ArrowDown', 40)
await sleep(80)
const emptyAt = await state()
await CTRL_B()
await sleep(120)
const afterEmpty = await state()
await CTRL_B()
await sleep(120)
const afterEmptyTwice = await state()
check(
  '6 空选区加粗：出现 ****，光标在中间；再按删掉、光标回原位',
  emptyAt.anchor === 2 &&
    afterEmpty.doc === 'a\n****\nb' &&
    afterEmpty.anchor === 4 &&
    afterEmptyTwice.doc === 'a\n\nb' &&
    afterEmptyTwice.anchor === 2,
  { emptyAt, afterEmpty, afterEmptyTwice },
)

/* ── 7. 有选区 ── */

const selectRange = async (doc, from, to) => {
  await setDoc(doc)
  await CTRL_HOME()
  if (from > 0) await RIGHT(from)
  if (to > from) await SHIFT_RIGHT(to - from)
  await sleep(60)
  return state()
}

const boldSel = await selectRange('abc', 0, 3)
await CTRL_B()
await sleep(120)
const bolded = await state()
await CTRL_B()
await sleep(120)
const unbolded = await state()
check(
  '7a 选中 abc 加粗 → **abc**，选区仍是 abc；再按回到 abc',
  boldSel.from === 0 && boldSel.to === 3 &&
    bolded.doc === '**abc**' && bolded.anchor === 2 && bolded.head === 5 &&
    unbolded.doc === 'abc' && unbolded.from === 0 && unbolded.to === 3,
  { boldSel, bolded, unbolded },
)

const italicSel = await selectRange('abc', 0, 3)
await CTRL_I()
await sleep(120)
const italicized = await state()
await CTRL_I()
await sleep(120)
const unitalicized = await state()
check(
  '7b 选中 abc 斜体 → *abc*，再按回到 abc',
  italicSel.from === 0 &&
    italicized.doc === '*abc*' && italicized.anchor === 1 && italicized.head === 4 &&
    unitalicized.doc === 'abc' && unitalicized.from === 0 && unitalicized.to === 3,
  { italicized, unitalicized },
)

const nestedSel = await selectRange('**abc**', 2, 5)
await CTRL_I()
await sleep(120)
const nested = await state()
check(
  '7c **abc** 上按斜体 → **_abc_**（不重排、不吞并成 ***）',
  nestedSel.from === 2 && nested.doc === '**_abc_**' && nested.anchor === 3 && nested.head === 6,
  { nestedSel, nested },
)

/* ── 8. 多行 ── */

await setDoc('a\n\nb')
await CTRL_HOME()
await CTRL_SHIFT_END()
await sleep(60)
const multiSel = await state()
await CTRL_B()
await sleep(120)
const multi = await state()
check(
  '8 选中 a\\n\\nb 加粗 → **a**\\n\\n**b**，空行原样',
  multiSel.from === 0 && multiSel.to === 4 && multi.doc === '**a**\n\n**b**',
  { multiSel, multi },
)

/* ── 9. 一次 undo 足够 ── */

await setDoc('abc')
await CTRL_HOME()
await SHIFT_RIGHT(3)
await CTRL_B()
await sleep(120)
const beforeUndo = await state()
await CTRL_Z()
await sleep(120)
const afterUndo = await state()
check(
  '9 一次 Mod+B 之后，一次 Mod+Z 回到原样',
  beforeUndo.doc === '**abc**' && afterUndo.doc === 'abc',
  { beforeUndo: beforeUndo.doc, afterUndo: afterUndo.doc },
)

const markShot = await shot('m2-7-marks')

/* ═══════════════════════ D. 重启进程（B-16） ═══════════════════════ */

// 落盘类检查必须从冷进程开始：槽位是进程级静态，上一段测试留下的 pending
// 会让「Mod+S 之后多久落盘」这个判据失去意义。
killApp()
await sleep(700)
// 夹具还原放在应用不在跑的时候，避免自动保存又把它写回去。
const MARK_FILE = path.join(VAULT, '标记.md')
const SAVE_FILE = path.join(VAULT, '保存.md')
fs.writeFileSync(MARK_FILE, 'abc', 'utf8')
fs.writeFileSync(SAVE_FILE, '初始', 'utf8')
for (const f of createdRoot) fs.rmSync(path.join(VAULT, f), { force: true })
for (const f of createdDiary) fs.rmSync(path.join(VAULT, '日记', f), { force: true })

bind(await openApp({ vault: VAULT, restart: true, errors: consoleErrors }))
await sleep(400)

const openedSave = await openNote('保存.md')
await sleep(300)

/* ── 10a Mod+S 落盘 ── */

await focusEditor()
await CTRL_END()
await insertText('X')
await sleep(80)
const tSave = Date.now()
await CTRL_S()
const flashShown = await waitFor(
  `document.querySelector('.viewer__meta')?.textContent?.includes('已保存') === true`,
  3000,
)
let savedAt = null
for (let i = 0; i < 40; i += 1) {
  if (fs.readFileSync(SAVE_FILE, 'utf8').includes('X')) {
    savedAt = Date.now() - tSave
    break
  }
  await sleep(50)
}
check(
  '10a Mod+S：出现「已保存」回执，且内容已落盘',
  openedSave === true && flashShown === true && savedAt !== null,
  { flashShown, savedAtMs: savedAt, disk: fs.readFileSync(SAVE_FILE, 'utf8') },
)

/* ── 10b 焦点隔离：浮层输入框里 Mod+B 不动文档 ── */

const docBeforeIsolation = await state()
await CTRL_P()
const paletteOpen = await waitFor(`document.querySelector('.quick-open') !== null`, 3000)
await insertText('保')
await CTRL_B()
await CTRL_I()
await sleep(200)
const docAfterIsolation = await state()
const paletteStill = await ev(`document.querySelector('.quick-open') !== null`)
const inputValue = await ev(`document.querySelector('.quick-open__input')?.value ?? null`)
check(
  '10b 快速打开浮层里按 Mod+B / Mod+I：文档一字未动',
  paletteOpen === true &&
    docAfterIsolation.doc === docBeforeIsolation.doc &&
    paletteStill === true &&
    inputValue === '保',
  {
    paletteOpen,
    before: docBeforeIsolation.doc,
    after: docAfterIsolation.doc,
    paletteStill,
    inputValue,
  },
)
await ESC()
await sleep(200)

/* ── 10c 旧键不回归 ── */

await focusEditor()
await CTRL_SHIFT_F()
const searchOpen = await waitFor(`document.querySelector('.search-panel') !== null`, 3000)
await ESC()
await sleep(200)
await CTRL_P()
const paletteAgain = await waitFor(`document.querySelector('.quick-open') !== null`, 3000)
await ESC()
await sleep(200)
const focusBack = await ev(`document.activeElement?.classList.contains('cm-content') === true`)
check(
  '10c Mod+P / Mod+Shift+F 行为不变（各开一次，Esc 后焦点回编辑器）',
  searchOpen === true && paletteAgain === true && focusBack === true,
  { searchOpen, paletteAgain, focusBack },
)

/* ── 10d 切主题不重建编辑器 ── */

await ev(`(() => {
  const el = document.querySelector('.cm-content')
  el.dataset.kiProbe = 'PROBE-M2-7'
  window.__kiNode = el
  return true
})()`)
const themeBefore = await ev(`document.documentElement.dataset.theme`)
const toggleRect = await ev(`(() => {
  const r = document.querySelector('.theme-toggle').getBoundingClientRect()
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
})()`)
// 三态循环里有一档点下去**颜色不变**（system 且系统主题恰好等于当前），
// 所以不能写"点一下就变"——按结果收敛，最多三轮（上一刀踩过这个坑）。
let themeClicks = 0
for (let i = 0; i < 3; i += 1) {
  if ((await ev(`document.documentElement.dataset.theme`)) !== themeBefore) break
  await clickAt(toggleRect.x, toggleRect.y)
  themeClicks += 1
  await sleep(200)
}
const themeProbe = await ev(`({
  theme: document.documentElement.dataset.theme,
  same: window.__kiNode === document.querySelector('.cm-content'),
  probe: document.querySelector('.cm-content')?.dataset.kiProbe ?? null,
})`)
check(
  '10d 切主题：dataset.theme 变化，.cm-content 仍是同一个 DOM 节点',
  themeProbe.theme !== themeBefore && themeProbe.same === true &&
    themeProbe.probe === 'PROBE-M2-7',
  { themeBefore, themeClicks, ...themeProbe },
)

const finalShot = await shot('m2-7-final')

/* ═══════════════════════ 收尾 ═══════════════════════ */

check('X 运行期无 console error / 未捕获异常', consoleErrors.length === 0, consoleErrors.slice(0, 5))

killApp()
await sleep(400)
fs.writeFileSync(SAVE_FILE, '初始', 'utf8')
fs.writeFileSync(MARK_FILE, 'abc', 'utf8')

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
console.log('')
console.log(`截图：${markShot} / ${finalShot}`)
console.log(
  `${failed.length === 0 ? '✅' : '❌'} 共 ${results.length} 项：${results.length - failed.length - skipped.length} 通过 / ${failed.length} 失败 / ${skipped.length} 跳过`,
)
if (skipped.length > 0) {
  console.log(`⚠ ${skipped.length} 项没有执行，本次验收不完整，不能当全绿看`)
}
fs.writeFileSync(
  path.join(OUT_DIR, `keys-acceptance-${Date.now()}.json`),
  JSON.stringify({ results, consoleErrors, stamp: Date.now() }, null, 2),
)
app.close()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
