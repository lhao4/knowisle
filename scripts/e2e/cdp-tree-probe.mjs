// M7 第十七刀（B-09 ②）· 「树会自己刷新」的端到端验收。真实 WebView2 + CDP。
//
// # 这一条在验什么
//
// 第十六刀给了手动重扫，但用户还是得**自己想起来**点一下。这一刀让树在窗口
// 重新获得焦点时自己对齐磁盘——靠的不是 watcher，也不是「根目录 mtime」
// （门禁②实测抓不到子目录里的任何增删），而是**目录指纹**：所有目录的相对
// 路径 + 每个目录下的条目名集合，哈希成一个 hex。
//
// 所以本脚本的判据全是「切走切回之后，树里有什么」：
//
//   1. 外部在**子目录**新建 → 切回来 → 自己出现（原方案抓不到的场景）
//   2. 外部删除非当前笔记 → 自己消失
//   3. 外部**改名** → 旧名消失、新名出现（比原方案多抓一种）
//   4. 自己编辑并等 autosave 落盘 5 次 → 切回来 → **不重扫**
//   5. 当前笔记被外部删除 → 自动重扫 + 提示自己出现，内容与撤销栈不动
//   6. 有未保存内容时自动重扫 → 内容 sha 逐字不变、Ctrl+Z 仍能撤销
//   7. 回收站面板开着时切回来 → 不重扫；关掉面板后补一次
//  10. 手动按钮仍工作；自动 + 手动同时来只扫 1 次
//
// 判据 8（探针 p50）不在这里：它是 Rust 侧的 readdir 代价，量尺是 ADR-018
// 门禁①（release，21 次采样）。判据 9（既有 e2e 一行断言不改）由 `e2e:all`
// 证明。
//
// # 「切走切回」怎么模拟
//
// `Page.bringToFront` 不会可靠地派发 `window.focus`（TOOLING），所以脚本
// 直接派发 `blur` → 等 1.2s → `focus`。**那 1.2 秒不是凑数**：探针只在
// 「离开过 ≥1s」时探（ADR-018 门禁①的 5~20ms 档处方），少于 1 秒的来回
// 切换刻意不探。这也让「切走切回」更像真人：Alt+Tab 出去看一眼前，再回来。
//
// # 为什么不用 KI_FP_CHANGED 当主判据
//
// 门禁③ 的 `console.count('KI_FP_CHANGED')` 是**临时插桩**，量完就删
// （ADR-018）。删掉之后这条判据不能变成永远为真的空断言，所以主判据换成
// **生产代码自己会做的事**：每次重扫（手动或自动）都会把 `.rescan-btn`
// 置成 `disabled`——脚本挂一个 MutationObserver 数这个跃迁，就数到了
// 「扫了几次」。插桩在的时候（开发期）额外核对 KI_FP_CHANGED，见 4d。
//
// # 前置（缺一条都不算数）
//
//   1. `pnpm dev`：vite 在 127.0.0.1:1420（脚本会取 /src/App.tsx 核对转译结果，B-18）
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. `--restart`（或 `pnpm e2e:tree-probe --restart`）：脚本自己杀进程再起
//   4. 运行期间源码冻结
//
// 夹具：`os.tmpdir()/knowisle-e2e-probe`（`prepareProbeVault`），独立于任何
// 别的脚本——本脚本会从 Node 侧新建目录 / 删笔记 / 改名笔记，共用库会让
// 下一个脚本的起点不确定（B-32）。
//
// 跑法：`pnpm e2e:tree-probe`（或加 `--restart`）
// 退出码只有三种（B-19）：0 全跑全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'
import crypto from 'node:crypto'

import { MOD, openApp } from './cdp.mjs'
import { OUT_DIR, PROBE_VAULT as VAULT, sleep } from './env.mjs'
import { prepareProbeVault } from './fixtures.mjs'

const STAMP = String(Date.now()).slice(-6)
const log = (...a) => console.log(...a)

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}

/* ───────────────────────── 会话 ───────────────────────── */

/** `console.count` 的每一次调用（只有插桩在的时候才有）。 */
const fpEvents = []
const consoleErrors = []
const app = await openApp({
  vault: VAULT,
  ready: '.tree-row',
  restart: true,
  prepare: prepareProbeVault,
  errors: consoleErrors,
  onEvent: (m) => {
    if (m.method !== 'Runtime.consoleAPICalled' || m.params.type !== 'count') return
    const label = m.params.args.map((a) => a.value ?? '').join(' ')
    if (label.startsWith('KI_FP_CHANGED')) fpEvents.push(label)
  },
})
const { ev, waitFor, press, insertText } = app

const root = await ev(`document.querySelector('.sidebar__root')?.title ?? null`)
log(`vault = ${root}`)
log(`stats = ${await ev(`document.querySelector('.stats')?.textContent ?? null`)}`)

/* ───────────────────────── 工具 ───────────────────────── */

const CTRL_S = () => press('s', 'KeyS', 83, MOD.ctrl)
const CTRL_Z = () => press('z', 'KeyZ', 90, MOD.ctrl)
const CTRL_END = () => press('End', 'End', 35, MOD.ctrl)
const ESC = () => press('Escape', 'Escape', 27)

const treeTitles = () => ev(`[...document.querySelectorAll('.tree-row')].map((el) => el.title)`)
const treeRowCount = () => ev(`document.querySelectorAll('.tree-row').length`)
const statsText = () => ev(`document.querySelector('.stats')?.textContent ?? null`)
const breadcrumb = () => ev(`document.querySelector('.breadcrumb')?.textContent ?? null`)
const errorBanner = () => ev(`document.querySelector('.banner--error')?.textContent ?? null`)
const focusEditor = () => ev(`document.querySelector('.cm-content')?.focus(); true`)

/**
 * 装一个「扫了几次」的计数器。
 *
 * 每次重扫都会把 `.rescan-btn` 置成 `disabled`（手动与自动共用 `rescanning`
 * 这个 state），数 `disabled` 的跃迁就等于数扫描次数。它是**生产行为**，
 * 不是插桩——插桩删掉之后这条判据依然成立。
 */
const installScanCounter = async () => {
  const ok = await ev(`(() => {
    const btn = document.querySelector('.rescan-btn')
    if (btn === null) return false
    window.__kiScans = 0
    new MutationObserver(() => {
      if (btn.disabled) window.__kiScans += 1
    }).observe(btn, { attributes: true, attributeFilter: ['disabled'] })
    return true
  })()`)
  return ok
}
const scanCount = () => ev(`window.__kiScans ?? -1`)
const fpCount = () => fpEvents.length

const expandedOf = (title) =>
  ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    return el === null ? null : el.getAttribute('aria-expanded')
  })()`)

const expandDir = async (title) => {
  if ((await expandedOf(title)) === 'true') return true
  const clicked = await ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    if (el === null) return false
    el.click()
    return true
  })()`)
  if (!clicked) return false
  return waitFor(
    `(() => {
      const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
      return el !== null && el.getAttribute('aria-expanded') === 'true'
    })()`,
    2500,
  )
}

/** 点树里的一行（打开笔记）。 */
const openNote = async (title) => {
  const ok = await ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    if (el === null) return false
    el.scrollIntoView({ block: 'center' })
    el.click()
    return true
  })()`)
  if (!ok) return false
  const opened = await waitFor(
    `document.querySelector('.breadcrumb')?.textContent === ${JSON.stringify(title)}`,
    6000,
  )
  if (opened) await sleep(180)
  return opened
}

/** 编辑器快照：DOM 身份探针 + 正文 sha + 长度。 */
const editorSnapshot = async (tag) => {
  await ev(`(() => { const el = document.querySelector('.cm-editor'); if (el) el.dataset.kiProbe = ${JSON.stringify(tag)}; return true })()`)
  return editorAfter()
}
const editorAfter = () =>
  ev(`(async () => {
    const editor = document.querySelector('.cm-editor')
    if (editor === null) return { open: false }
    const text = editor.querySelector('.cm-content')?.innerText ?? ''
    const bytes = new TextEncoder().encode(text)
    const digest = await crypto.subtle.digest('SHA-256', bytes)
    return {
      open: true,
      probe: editor.dataset.kiProbe ?? null,
      len: text.length,
      sha: [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join(''),
      tail: text.slice(-40),
    }
  })()`)

/** 点侧栏的「重新扫描」并等它跑完。 */
const clickRescan = async () => {
  const clicked = await ev(`(() => {
    const b = document.querySelector('.rescan-btn')
    if (b === null) return false
    b.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    return true
  })()`)
  if (!clicked) return { ok: false, reason: '没有 .rescan-btn' }
  await waitFor(`document.querySelector('.rescan-btn')?.disabled === true`, 2000)
  const done = await waitFor(
    `(() => {
      const b = document.querySelector('.rescan-btn')
      return b !== null && b.disabled === false && b.textContent.includes('重新扫描')
    })()`,
    120000,
  )
  await sleep(250)
  return { ok: done }
}

/**
 * 切走 → 1.2s → 切回，并等自动重扫落地。
 *
 * `expect` 是**期望扫几次**（0 = 期望一次都不扫）。等到位就返回，绝不靠
 * 「睡够了应该就完了」——那会让 4/7 的「不该扫」变成永远为真的空断言。
 */
const awayBack = async (expect) => {
  const before = await scanCount()
  const fpBefore = fpCount()
  await ev(`window.dispatchEvent(new Event('blur')); true`)
  await sleep(1200)
  await ev(`window.dispatchEvent(new Event('focus')); true`)
  if (expect > 0) {
    await waitFor(`(window.__kiScans ?? 0) >= ${before + expect}`, 20000)
  }
  await waitFor(`document.querySelector('.rescan-btn')?.disabled === false`, 20000)
  await sleep(350)
  return { before, after: await scanCount(), fpBefore, fpAfter: fpCount() }
}

/* ───────────────────────── 准备 ───────────────────────── */

// `notes` 9 个子项 → 默认折叠（`depth === 0 && children <= 8` 才展开）。
await expandDir('notes')
const opened = await openNote('notes/alpha.md')
await focusEditor()
await CTRL_END()
await insertText('基线标记-甲')
await CTRL_S()
await sleep(900)

check('0a 夹具与当前笔记就位', opened && (await breadcrumb()) === 'notes/alpha.md', {
  root,
  stats: await statsText(),
  rows: await treeRowCount(),
  diskHasMarker: fs.readFileSync(path.join(VAULT, 'notes/alpha.md'), 'utf8').includes('基线标记-甲'),
})
check('0b 扫描计数器装上了（数 .rescan-btn 的 disabled 跃迁）', await installScanCounter(), {
  scans: await scanCount(),
})

/* ═══════════ 判据 1：外部在子目录新建 → 切回来自己出现 ═══════════ */

{
  const dir = path.join(VAULT, 'notes/新来的')
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(path.join(dir, '第一篇.md'), '# 第一篇\n\n在应用之外建的。\n', 'utf8')
  await sleep(150)

  const before = await treeTitles()
  const run = await awayBack(1)
  const after = await treeTitles()

  check('1a 新建前树里没有这个目录', !before.includes('notes/新来的') && !before.includes('notes/新来的/第一篇.md'), {
    hasDir: before.includes('notes/新来的'),
  })
  check('1b 切回来之后目录自己出现（没点按钮）', run.after === run.before + 1 && after.includes('notes/新来的'), {
    scans: [run.before, run.after],
    hasDir: after.includes('notes/新来的'),
  })
  // 新目录是 depth 1 → 默认折叠。先展开再断言文件行：`notes/新来的` 这一行
  // 出现只证明「目录被扫到了」，文件行才证明「子树也被扫到了」。
  const expanded = await expandDir('notes/新来的')
  check('1c 展开之后文件行也在', expanded && (await treeTitles()).includes('notes/新来的/第一篇.md'), {
    expanded,
    hasFile: (await treeTitles()).includes('notes/新来的/第一篇.md'),
  })
}

/* ═══════════ 判据 2：外部删除非当前笔记 → 自己消失 ═══════════ */

{
  fs.unlinkSync(path.join(VAULT, 'notes/beta.md'))
  const before = await treeTitles()
  const run = await awayBack(1)
  const after = await treeTitles()

  check('2a 删除前树里有它', before.includes('notes/beta.md'), { has: before.includes('notes/beta.md') })
  check('2b 切回来之后自己消失', run.after === run.before + 1 && !after.includes('notes/beta.md'), {
    scans: [run.before, run.after],
    has: after.includes('notes/beta.md'),
  })
  check('2c 当前笔记不变', (await breadcrumb()) === 'notes/alpha.md', await breadcrumb())
}

/* ═══════════ 判据 3：外部改名 → 旧名消失、新名出现 ═══════════ */

{
  fs.renameSync(path.join(VAULT, 'notes/gamma.md'), path.join(VAULT, 'notes/gamma-改名.md'))
  const run = await awayBack(1)
  const after = await treeTitles()

  check('3a 旧名消失', run.after === run.before + 1 && !after.includes('notes/gamma.md'), {
    scans: [run.before, run.after],
    has: after.includes('notes/gamma.md'),
  })
  check('3b 新名出现（原方案「不抓改名」的反面）', after.includes('notes/gamma-改名.md'), {
    has: after.includes('notes/gamma-改名.md'),
  })
}

/* ═══════════ 判据 4：自己保存 5 次 → 切回来不重扫 ═══════════ */

{
  const rowsBefore = await treeRowCount()
  const statsBefore = await statsText()
  const fpBefore = fpCount()

  // 1.5s 节拍 + 一点余量：每一轮都等到真的落盘，而不是"发了编辑就算"。
  for (let i = 0; i < 5; i += 1) {
    await focusEditor()
    await CTRL_END()
    await insertText(`保存-${i}`)
    await sleep(1700)
  }

  const run = await awayBack(0)
  const rowsAfter = await treeRowCount()
  const statsAfter = await statsText()

  check('4a 5 次自动落盘之后切回来不重扫', run.after === run.before, {
    scans: [run.before, run.after],
  })
  check('4b 树行数不变', rowsAfter === rowsBefore, { before: rowsBefore, after: rowsAfter })
  // 反面：`.stats` 里的 `elapsedMs` 每次扫描都会换，所以「没换」是一条
  // 独立于扫描计数器的证据（万一 MutationObserver 漏了跃迁）。
  check('4c 顶栏统计没换（elapsedMs 没刷新）', statsAfter === statsBefore, {
    before: statsBefore,
    after: statsAfter,
  })
  check('4d KI_FP_CHANGED 计数为 0（插桩在时才有意义）', run.fpAfter === fpBefore, {
    before: fpBefore,
    after: run.fpAfter,
    instrumented: fpEvents.length > 0,
  })
}

/* ═══ 判据 4 的另一半：应用自己往 `.knowisle` 里写，不许触发自己 ═══ */

// 回收站就是 `.knowisle/trash/<ts>-名字.md`——**是 .md**，扩展名判据拦不住它，
// 只有「指纹与扫描器共用同一份 IgnoreRules」才挡得住。变异②（指纹不排除
// `.knowisle`）会在这里红；而 4a 只测「改内容」是红不了的（索引库不是笔记）。
{
  const rowsBefore = await treeRowCount()
  const trashDir = path.join(VAULT, '.knowisle', 'trash')
  fs.mkdirSync(trashDir, { recursive: true })
  const victim = path.join(trashDir, `${Date.now()}-probe.md`)
  fs.writeFileSync(victim, '# 被删掉的那篇\n\n正文。\n', 'utf8')

  const run = await awayBack(0)
  const rowsAfter = await treeRowCount()

  check('4e 应用自己写 .knowisle/trash/*.md 不重扫（忽略规则挡住的）', run.after === run.before, {
    scans: [run.before, run.after],
    trashFile: path.basename(victim),
  })
  check('4f 回收站里的 .md 没有混进树', rowsAfter === rowsBefore, {
    before: rowsBefore,
    after: rowsAfter,
  })

  fs.unlinkSync(victim)
}

/* ═══════════ 判据 5：当前笔记被外部删除 ═══════════ */

{
  const tag = `probe-${STAMP}`
  const snapBefore = await editorSnapshot(tag)
  // 先备份**当前**磁盘内容（含刚才那 5 个标记）：等一下要原样放回去，
  // 否则「磁盘 vs 编辑器」会分叉，后面的判据会多出一条无关的横幅。
  const backup = path.join(OUT_DIR, `probe-alpha-${STAMP}.md`)
  fs.copyFileSync(path.join(VAULT, 'notes/alpha.md'), backup)

  fs.unlinkSync(path.join(VAULT, 'notes/alpha.md'))
  const run = await awayBack(1)
  const snapAfter = await editorAfter()
  const notice = await ev(`document.querySelector('.banner--missing')?.textContent ?? null`)

  check('5a 自动重扫（没点按钮）', run.after === run.before + 1, { scans: [run.before, run.after] })
  check('5b 树里没有当前笔记', !(await treeTitles()).includes('notes/alpha.md'), {
    has: (await treeTitles()).includes('notes/alpha.md'),
  })
  check('5c 提示自己出现，且说明「不在」与「重新创建」的出口', notice !== null && notice.includes('不在') && notice.includes('重新创建'), { notice })
  check(
    '5d 编辑器内容逐字不变（同一 sha、同一长度）',
    snapBefore.open && snapAfter.open && snapBefore.sha === snapAfter.sha && snapBefore.len === snapAfter.len,
    { before: [snapBefore.sha, snapBefore.len], after: [snapAfter.sha, snapAfter.len] },
  )
  check('5e 编辑器 DOM 没重建（撤销栈还在的前提）', snapAfter.probe === tag, { probe: snapAfter.probe })

  // 把文件原样放回去，再用**手动按钮**把它带回树里（判据 10 的一半）。
  fs.copyFileSync(backup, path.join(VAULT, 'notes/alpha.md'))
  const manual = await clickRescan()
  check(
    '5f 手动按钮把它带回树里，提示随之消失',
    manual.ok && (await treeTitles()).includes('notes/alpha.md') && (await ev(`document.querySelector('.banner--missing') === null`)),
    { has: (await treeTitles()).includes('notes/alpha.md') },
  )

  // 聚焦校验也会为同一次删除挂一条「已不在磁盘上」的横幅。它不影响后面的
  // 判据，但留着会让人误以为「自动重扫之后内容被换了」——点掉它。
  await ev(`(() => {
    const btn = [...document.querySelectorAll('.banner .btn')].find((b) => b.textContent.trim() === '保留我的')
    if (btn) btn.click()
    return true
  })()`)
  await sleep(200)
}

/* ═══════════ 判据 6：有未保存内容时自动重扫 ═══════════ */

{
  await focusEditor()
  await CTRL_END()
  await insertText('未保存-乙')
  const tag = `probe-unsaved-${STAMP}`
  const snapBefore = await editorSnapshot(tag)

  // 外部新增一篇（落在已展开的 `inbox` 里）→ 切回来必然触发一次自动重扫。
  fs.writeFileSync(path.join(VAULT, 'inbox/三封.md'), '# 第三封\n\n外部新增。\n', 'utf8')
  const run = await awayBack(1)
  const snapAfter = await editorAfter()

  check('6a 自动重扫发生了', run.after === run.before + 1, { scans: [run.before, run.after] })
  check(
    '6b 编辑器内容逐字不变（有未保存内容也不许动）',
    snapBefore.open && snapAfter.open && snapBefore.sha === snapAfter.sha && snapBefore.len === snapAfter.len,
    { before: [snapBefore.sha, snapBefore.len], after: [snapAfter.sha, snapAfter.len] },
  )
  check('6c 编辑器 DOM 没重建', snapAfter.probe === tag, { probe: snapAfter.probe })

  const preUndo = await ev(`document.querySelector('.cm-content')?.innerText ?? ''`)
  await focusEditor()
  await CTRL_Z()
  await sleep(300)
  const postUndo = await ev(`document.querySelector('.cm-content')?.innerText ?? ''`)
  check(
    '6d Ctrl+Z 仍能撤销（撤销栈没被清）',
    preUndo.includes('未保存-乙') && !postUndo.includes('未保存-乙'),
    { before: preUndo.slice(-24), after: postUndo.slice(-24) },
  )

  // 收尾：把撤销之后的那一版落盘，后面的判据要一个干净的编辑器。
  await focusEditor()
  await CTRL_S()
  await sleep(900)
}

/* ═══════════ 判据 7：回收站面板开着时切回来不重扫 ═══════════ */

{
  const before = await scanCount()
  await ev(`document.querySelector('.trash-link').click(); true`)
  const openedPanel = await waitFor(`document.querySelector('.trash-panel') !== null`, 4000)

  fs.writeFileSync(path.join(VAULT, 'inbox/四封.md'), '# 第四封\n\n面板开着时新增。\n', 'utf8')
  const run = await awayBack(0)

  check('7a 回收站面板能打开', openedPanel, { opened: openedPanel })
  check('7b 面板开着时切回来不重扫', run.after === before, { scans: [before, run.after] })
  check('7c 树里还没有那个文件', !(await treeTitles()).includes('inbox/四封.md'), {
    has: (await treeTitles()).includes('inbox/四封.md'),
  })

  await ESC()
  const closed = await waitFor(`document.querySelector('.trash-panel') === null`, 3000)
  const caught = await waitFor(`(window.__kiScans ?? 0) >= ${before + 1}`, 20000)
  await sleep(400)

  check('7d 关掉面板之后补了一次', closed && caught && (await scanCount()) === before + 1, {
    closed,
    caught,
    scans: [before, await scanCount()],
  })
  check('7e 补的那一次把文件带进树里', (await treeTitles()).includes('inbox/四封.md'), {
    has: (await treeTitles()).includes('inbox/四封.md'),
  })
}

/* ═══════════ 判据 10：手动按钮仍工作；自动 + 手动只扫一次 ═══════════ */

{
  const before = await scanCount()
  const manual = await clickRescan()
  check('10a 手动「重新扫描」仍工作', manual.ok && (await scanCount()) === before + 1, {
    scans: [before, await scanCount()],
  })

  // 自动 + 手动同时来：先点按钮占住闸门，紧接着派发一次 focus。
  // 探针这一路会算出「变了」，然后调 `rescanVault` —— 被 ref 闸门挡回去。
  fs.writeFileSync(path.join(VAULT, 'inbox/五封.md'), '# 第五封\n\n同时来。\n', 'utf8')
  const before2 = await scanCount()
  await ev(`document.querySelector('.rescan-btn').click(); true`)
  await ev(`window.dispatchEvent(new Event('focus')); true`)
  await waitFor(`(window.__kiScans ?? 0) >= ${before2 + 1}`, 20000)
  await waitFor(`document.querySelector('.rescan-btn')?.disabled === false`, 20000)
  await sleep(600)
  const after2 = await scanCount()

  check('10b 自动 + 手动同时来只扫 1 次（共用 rescanGate）', after2 === before2 + 1, {
    scans: [before2, after2],
  })
  check('10c 两次触发之后树仍然正确', (await treeTitles()).includes('inbox/五封.md'), {
    has: (await treeTitles()).includes('inbox/五封.md'),
    rows: await treeRowCount(),
  })
  check('10d 全程没有错误横幅', (await errorBanner()) === null, { error: await errorBanner() })
}

/* ───────────────────────────── 汇总 ───────────────────────────── */

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
const passed = results.filter((r) => r.ok === true)

console.log('\n──────── 汇总 ────────')
console.log(`PASS ${passed.length} / FAIL ${failed.length} / SKIP ${skipped.length}`)
for (const f of failed) console.log(`  ✗ ${f.name}: ${JSON.stringify(f.detail)}`)
if (skipped.length > 0) console.log(`⚠ ${skipped.length} 项没有执行，本次验收不完整，不能当全绿看`)
if (consoleErrors.length > 0) console.log(`页面报错 ${consoleErrors.length} 条：\n${consoleErrors.join('\n')}`)
log(`KI_FP_CHANGED 事件 ${fpEvents.length} 条（插桩在时才有；最终提交里没有插桩）`)

fs.writeFileSync(
  path.join(OUT_DIR, `tree-probe-acceptance-${STAMP}.json`),
  JSON.stringify({ results, consoleErrors, fpEvents, stamp: STAMP }, null, 2),
)
app.close()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
