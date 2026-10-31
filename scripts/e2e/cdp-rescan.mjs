// M7 第十六刀（B-09 ①）· 「重新扫描」的端到端验收。真实 WebView2 + CDP。
//
// 本脚本要证明的不是「按钮能点」，而是**重扫之后账要对得上**：
//
//   1. 外部新建：树里先没有 → 点重扫 → 出现，且默认展开规则让那一行可见
//   2. 外部删除（非当前笔记）：树里消失，当前笔记不变
//   3. 展开态保留：手动展开的目录仍展开，没动过的仍折叠
//   4. 当前笔记被外部删除：出提示、编辑器逐字不变、撤销栈还在、保存被拒绝
//   5. 回收站停在第 2 页 → 重扫 → 仍在第 2 页
//   6. 连点 5 次：按钮 disabled、无报错、最终树正确、不卡在「扫描中…」
//   7. 重扫期间搜索面板仍可用（不报错）
//   8. 失败可见：库目录没了 → 横幅报错，**旧树不被清空**
//
// # 判据 4 的「保存会怎样」是量出来的，不是推的
//
// 门禁②（改前）实测：外部删掉当前笔记之后，`note_status` 仍报 `dirty:false`、
// `lastError:null`、`lastSavedMtime` 是旧值——槽位根本不知道文件没了。所以
// **保存不会被静默写成新建**：`write_note_checked` 把「文件不存在」也算冲突
// （`diskMtime: null`），`describeError` 已经有一句「磁盘上已经没有这个文件了」。
// 真正会复活那个路径的是冲突横幅上的「用我的版本覆盖」。
//
// 于是本脚本的判据 4 分三段：① 重扫后出提示、内容与撤销栈不动；② 保存被
// 拒绝，磁盘上**没有**被静默写回来；③ 点「用我的版本覆盖」才会重新创建它
// ——提示里的那句话必须对得上第三段。
//
// # 前置（缺一条都不算数）
//
//   1. `pnpm dev`：vite 在 127.0.0.1:1420（脚本会取 /src/App.tsx 核对转译结果，B-18）
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. `--restart`（或 `pnpm e2e:rescan --restart`）：脚本自己杀进程再起
//   4. 运行期间源码冻结
//
// 夹具：`os.tmpdir()/knowisle-e2e-rescan`（`prepareRescanVault`），独立于
// 任何别的脚本——本脚本会从 Node 侧新建目录 / 删笔记 / 改名笔记 / 改名库目录，
// 共用库会让下一个脚本的起点不确定（B-32）。
//
// 跑法：`pnpm e2e:rescan`（或加 `--restart`）
// 退出码只有三种（B-19）：0 全跑全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'
import crypto from 'node:crypto'
import { execFileSync } from 'node:child_process'

import { MOD, openApp } from './cdp.mjs'
import { OUT_DIR, RESCAN_VAULT as VAULT, sleep } from './env.mjs'
import { prepareRescanVault } from './fixtures.mjs'

const STAMP = String(Date.now()).slice(-6)
const log = (...a) => console.log(...a)

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}
const skip = (name, reason) => {
  results.push({ name, ok: null, detail: reason })
  log(`SKIP  ${name}  ${reason}`)
}

const sha = (text) => crypto.createHash('sha256').update(text, 'utf8').digest('hex')
const diskText = (rel) => {
  const abs = path.join(VAULT, rel)
  return fs.existsSync(abs) ? fs.readFileSync(abs, 'utf8') : null
}

/* ───────────────────────── 会话 ───────────────────────── */

const consoleErrors = []
const app = await openApp({
  vault: VAULT,
  ready: '.tree-row',
  restart: true,
  prepare: prepareRescanVault,
  errors: consoleErrors,
})
const { ev, waitFor, press, insertText, send } = app

const root = await ev(`document.querySelector('.sidebar__root')?.title ?? null`)
log(`vault = ${root}`)
log(`stats = ${await ev(`document.querySelector('.stats')?.textContent ?? null`)}`)

/* ───────────────────────── 工具 ───────────────────────── */

const CTRL_S = () => press('s', 'KeyS', 83, MOD.ctrl)
const CTRL_Z = () => press('z', 'KeyZ', 90, MOD.ctrl)
const CTRL_END = () => press('End', 'End', 35, MOD.ctrl)
const CTRL_SHIFT_F = () => press('f', 'KeyF', 70, MOD.ctrl | MOD.shift)
const ESC = () => press('Escape', 'Escape', 27)

const treeTitles = () => ev(`[...document.querySelectorAll('.tree-row')].map((el) => el.title)`)
const treeRowCount = () => ev(`document.querySelectorAll('.tree-row').length`)
const banners = () => ev(`[...document.querySelectorAll('.banner')].map((el) => el.textContent.trim())`)
const breadcrumb = () => ev(`document.querySelector('.breadcrumb')?.textContent ?? null`)

const expandedOf = (title) =>
  ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    return el === null ? null : el.getAttribute('aria-expanded')
  })()`)

const expandDir = async (title) => {
  if ((await expandedOf(title)) === 'true') return true
  const ok = await ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    if (el === null) return false
    el.click()
    return true
  })()`)
  if (!ok) return false
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

const focusEditor = () => ev(`document.querySelector('.cm-content')?.focus(); true`)

/** 编辑器快照：DOM 身份探针 + 正文 sha + 长度。 */
const editorSnapshot = async (tag) => {
  await ev(`(() => { const el = document.querySelector('.cm-editor'); if (el) el.dataset.kiProbe = ${JSON.stringify(tag)}; return true })()`)
  return ev(`(async () => {
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

/** 按钮状态：文案 + disabled + aria-label。 */
const rescanState = () =>
  ev(`(() => {
    const b = document.querySelector('.rescan-btn')
    if (b === null) return { present: false }
    return {
      present: true,
      text: b.textContent.trim(),
      label: b.getAttribute('aria-label'),
      disabled: b.disabled === true,
    }
  })()`)

/** 点侧栏的「重新扫描」并等它跑完。 */
const rescan = async ({ via = 'click' } = {}) => {
  const clicked = await ev(`(() => {
    const b = document.querySelector('.rescan-btn')
    if (b === null) return false
    if (${JSON.stringify(via)} === 'dom') b.click()
    else b.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    return true
  })()`)
  if (!clicked) return { ok: false, reason: '没有 .rescan-btn' }
  const busy = await waitFor(`document.querySelector('.rescan-btn')?.disabled === true`, 2000)
  const done = await waitFor(
    `(() => {
      const b = document.querySelector('.rescan-btn')
      return b !== null && b.disabled === false && b.textContent.includes('重新扫描')
    })()`,
    120000,
  )
  await sleep(250)
  return { ok: done, busy, state: await rescanState() }
}

/* ───────────────────────── 准备 ───────────────────────── */

// `notes` 20 个子项 → 默认折叠（`depth === 0 && children <= 8` 才展开）。
await expandDir('notes')
const opened = await openNote('notes/alpha.md')
await focusEditor()
await CTRL_END()
await insertText('基线标记-甲')
await CTRL_S()
await sleep(900)

check('0a 夹具与当前笔记就位', opened && (await breadcrumb()) === 'notes/alpha.md', {
  root,
  stats: await ev(`document.querySelector('.stats')?.textContent ?? null`),
  rows: await treeRowCount(),
  diskHasMarker: (diskText('notes/alpha.md') ?? '').includes('基线标记-甲'),
})
check('0b 侧栏有「重新扫描」且初始可点', (await rescanState()).present && (await rescanState()).disabled === false, await rescanState())

/* ═══════════ 判据 1：外部新建 → 重扫 → 树里出现 ═══════════ */

{
  const rel = '新来的/第一篇.md'
  fs.mkdirSync(path.join(VAULT, '新来的'), { recursive: true })
  fs.writeFileSync(path.join(VAULT, rel), '# 第一篇\n\n在应用之外建的。\n', 'utf8')
  await sleep(150)

  const before = await treeTitles()
  const run = await rescan()

  const after = await treeTitles()
  check('1a 外部新建的文件重扫前不在树里', !before.includes('新来的') && !before.includes(rel), {
    hasDir: before.includes('新来的'),
    hasFile: before.includes(rel),
  })
  check('1b 重扫后目录与文件都进树', run.ok && after.includes('新来的') && after.includes(rel), {
    run,
    hasDir: after.includes('新来的'),
    hasFile: after.includes(rel),
  })
  // 这一条**不是**重扫的功劳：`TreeNode` 的默认展开规则是
  // `depth === 0 && children <= 8`，新目录只有 1 个子项。写在这里是为了让
  // 「树里出现」与「眼睛能看见」分开——只有前者成立时，用户还是找不到它。
  check('1c 新目录按默认规则展开，那一行可见', (await expandedOf('新来的')) === 'true', {
    expanded: await expandedOf('新来的'),
  })
}

/* ═══════════ 判据 2：外部删除（非当前笔记） ═══════════ */

{
  const rel = 'notes/beta.md'
  fs.unlinkSync(path.join(VAULT, rel))
  const before = await treeTitles()
  const run = await rescan()
  const after = await treeTitles()

  check('2a 删除前树里有它', before.includes(rel), { has: before.includes(rel) })
  check('2b 重扫后树里消失', run.ok && !after.includes(rel), { run, has: after.includes(rel) })
  check('2c 当前笔记不变', (await breadcrumb()) === 'notes/alpha.md', await breadcrumb())
}

/* ═══════════ 判据 3：展开态保留 ═══════════ */

{
  const notesBefore = await expandedOf('notes')
  const subBefore = await expandedOf('deep/sub')
  const run = await rescan()
  const notesAfter = await expandedOf('notes')
  const subAfter = await expandedOf('deep/sub')

  check('3a 手动展开的目录重扫后仍展开', notesBefore === 'true' && notesAfter === 'true' && run.ok, {
    before: notesBefore,
    after: notesAfter,
  })
  // 反面：没动过的目录不许被重扫弹开。少了这一条，「重扫强制全展开」也能让 3a 绿。
  check('3b 没动过的折叠目录重扫后仍折叠', subBefore === 'false' && subAfter === 'false', {
    before: subBefore,
    after: subAfter,
  })
}

/* ═══════════ 判据 4：当前笔记被外部删除 ═══════════ */

{
  const beforeSnap = await editorSnapshot(`rescan-${STAMP}`)
  const beforeText = await ev(`document.querySelector('.cm-content')?.innerText ?? ''`)

  fs.unlinkSync(path.join(VAULT, 'notes/alpha.md'))
  const run = await rescan()
  await sleep(300)

  const afterSnap = await editorAfter()
  const notice = await ev(`document.querySelector('.banner--missing')?.textContent ?? null`)
  const afterText = await ev(`document.querySelector('.cm-content')?.innerText ?? ''`)

  check('4a 重扫后树里没有当前笔记', run.ok && !(await treeTitles()).includes('notes/alpha.md'), {
    run,
    has: (await treeTitles()).includes('notes/alpha.md'),
  })
  check(
    '4b 出提示，且说明「不在」与「重新创建」的出口',
    notice !== null && notice.includes('不在') && notice.includes('重新创建'),
    { notice },
  )
  check(
    '4c 编辑器内容逐字不变（同一 sha、同一长度）',
    beforeSnap.open && afterSnap.open && beforeSnap.sha === afterSnap.sha && beforeSnap.len === afterSnap.len,
    { before: [beforeSnap.sha, beforeSnap.len], after: [afterSnap.sha, afterSnap.len], same: beforeText === afterText },
  )
  check('4d 编辑器 DOM 没重建（撤销栈还在的前提）', afterSnap.probe === `rescan-${STAMP}`, {
    probe: afterSnap.probe,
  })

  // 撤销：一次 Ctrl+Z 撤掉基线里那个标记。重建过的编辑器不会响应。
  const preUndo = (await ev(`document.querySelector('.cm-content')?.innerText ?? ''`)).trimEnd()
  await focusEditor()
  await CTRL_Z()
  await sleep(250)
  const postUndo = (await ev(`document.querySelector('.cm-content')?.innerText ?? ''`)).trimEnd()
  check('4e Ctrl+Z 仍能撤销（撤销栈没被清）', preUndo.includes('基线标记-甲') && !postUndo.includes('基线标记-甲'), {
    before: preUndo.slice(-24),
    after: postUndo.slice(-24),
  })

  // 保存被拒绝：磁盘上不会自动出现这篇（门禁②实测的契约）。
  await focusEditor()
  await CTRL_END()
  await insertText('乙')
  await CTRL_S()
  await sleep(2400)
  const afterSave = await banners()
  check('4f 保存被拒绝，磁盘上没有静默复活', diskText('notes/alpha.md') === null, {
    disk: diskText('notes/alpha.md'),
    banners: afterSave,
  })
  check(
    '4g 冲突横幅给出「用我的版本覆盖」这个出口',
    afterSave.some((text) => text.includes('用我的版本覆盖')),
    afterSave,
  )

  // 提示里那句话必须对得上：点「覆盖」才重新创建它。
  const override = await ev(`(() => {
    const btn = [...document.querySelectorAll('.banner .btn')].find((b) => b.textContent.trim() === '用我的版本覆盖')
    if (!btn) return false
    btn.click()
    return true
  })()`)
  await sleep(1200)
  const recreated = diskText('notes/alpha.md')
  check('4h 「用我的版本覆盖」才会重新创建它', override && recreated !== null && recreated.includes('乙'), {
    override,
    tail: recreated === null ? null : recreated.slice(-24),
  })

  // 把这一篇放回树里，后面的判据要一个干净的当前笔记。
  await rescan()
  await sleep(200)
  check('4i 再重扫一次，它回到树里（提示随之消失）', (await treeTitles()).includes('notes/alpha.md') && (await ev(`document.querySelector('.banner--missing') === null`)), {
    has: (await treeTitles()).includes('notes/alpha.md'),
    notice: await ev(`document.querySelector('.banner--missing')?.textContent ?? null`),
  })
}

/* ═══════════ 判据 5：回收站停在第 2 页 ═══════════ */

{
  await ev(`document.querySelector('.trash-link').click(); true`)
  await waitFor(`document.querySelector('.trash-panel') !== null`, 3000)
  await waitFor(`document.querySelectorAll('.trash__item').length > 0`, 10000)
  const first = await ev(`document.querySelectorAll('.trash__item').length`)
  const more = await ev(`(() => {
    const btn = [...document.querySelectorAll('.trash-panel .btn')].find((b) => b.textContent.trim() === '加载更多')
    if (!btn) return false
    btn.click()
    return true
  })()`)
  const loaded = await waitFor(`document.querySelectorAll('.trash__item').length > ${first}`, 10000)
  const second = await ev(`document.querySelectorAll('.trash__item').length`)
  const progressBefore = await ev(`document.querySelector('.trash__progress')?.textContent ?? null`)

  // 面板是模态，真鼠标点不到侧栏（`.trash-backdrop` 盖住全屏）。这里用
  // `element.click()` 走**同一个** React 处理函数——判据问的是状态保留，
  // 不是命中测试。
  await ev(`document.querySelector('.rescan-btn').click(); true`)
  await waitFor(
    `(() => { const b = document.querySelector('.rescan-btn'); return b !== null && b.disabled === false && b.textContent.includes('重新扫描') })()`,
    120000,
  )
  await sleep(600)

  const third = await ev(`document.querySelectorAll('.trash__item').length`)
  const progressAfter = await ev(`document.querySelector('.trash__progress')?.textContent ?? null`)
  check('5a 回收站先翻到第 2 页（> 每页 200）', more && loaded && first === 200 && second > 200, {
    first,
    second,
    progressBefore,
  })
  check('5b 重扫后仍在第 2 页（行数不变）', third === second, {
    before: second,
    after: third,
    progressBefore,
    progressAfter,
  })

  await ESC()
  await waitFor(`document.querySelector('.trash-panel') === null`, 2000)
}

/* ═══════════ 判据 6：连点 5 次 ═══════════ */

{
  const errorsBefore = consoleErrors.length
  // 同一个任务里连点 5 次：`disabled` 要等下一次渲染才生效，中间那几下
  // 是真的会进 onClick 的——挡住它们的是 ref 闸门，不是 state。
  await ev(`(() => {
    const b = document.querySelector('.rescan-btn')
    for (let i = 0; i < 5; i += 1) b.click()
    return true
  })()`)
  const disabledAfterFirst = await waitFor(`document.querySelector('.rescan-btn')?.disabled === true`, 2000)
  // 这一份是「扫描进行中」的快照，只给 6a 当证据用——上面那句 waitFor 之后
  // 才取，别用 `done` 之后的状态冒充（那时按钮早就回到「重新扫描」了）。
  const stateDuringScan = await rescanState()
  const done = await waitFor(
    `(() => { const b = document.querySelector('.rescan-btn'); return b !== null && b.disabled === false && b.textContent.includes('重新扫描') })()`,
    120000,
  )
  await sleep(300)
  const titles = await treeTitles()
  const errorsAfter = consoleErrors.length

  check('6a 第一次点击后按钮 disabled', disabledAfterFirst, stateDuringScan)
  check('6b 最终不卡在「扫描中…」', done && (await rescanState()).text.includes('重新扫描'), await rescanState())
  check('6c 全程无 console 报错', errorsAfter === errorsBefore, {
    before: errorsBefore,
    after: errorsAfter,
    last: consoleErrors.slice(-3),
  })
  check('6d 最终树正确（夹具与外部新建都还在）', titles.includes('notes/alpha.md') && titles.includes('新来的/第一篇.md'), {
    rows: titles.length,
    hasAlpha: titles.includes('notes/alpha.md'),
    hasNew: titles.includes('新来的/第一篇.md'),
  })

  // 把 disabled 强行抹掉再连点：这一下只有 ref 闸门挡得住。不数 IPC
  // （那是门禁③的事，要临时插桩），只断言「不卡、不炸、树还对」。
  const errorsBefore2 = consoleErrors.length
  await ev(`(() => {
    const b = document.querySelector('.rescan-btn')
    for (let i = 0; i < 5; i += 1) { b.disabled = false; b.click() }
    return true
  })()`)
  const done2 = await waitFor(
    `(() => { const b = document.querySelector('.rescan-btn'); return b !== null && b.disabled === false && b.textContent.includes('重新扫描') })()`,
    120000,
  )
  await sleep(300)
  check('6e 抹掉 disabled 后连点也不卡（ref 闸门）', done2 && consoleErrors.length === errorsBefore2, {
    state: await rescanState(),
    errors: consoleErrors.length - errorsBefore2,
    rows: await treeRowCount(),
  })
}

/* ═══════════ 判据 7：重扫期间搜索仍可用 ═══════════ */

{
  const errorsBefore = consoleErrors.length
  // 重扫会让索引失效，`bulk/` 600 篇的同步窗口足够宽，宽到能在里面打开搜索。
  await ev(`document.querySelector('.rescan-btn').click(); true`)
  await CTRL_SHIFT_F()
  const opened = await waitFor(`document.querySelector('.search__input') !== null`, 4000)
  if (opened) {
    await ev(`document.querySelector('.search__input').focus(); true`)
    await insertText('索引同步的填充篇')
  }
  // 等「结果那一版」footer（B-22）：要么出命中，要么出「索引还在建立」。
  await waitFor(
    `(() => {
      const panel = document.querySelector('.search-panel')
      if (panel === null) return false
      const items = panel.querySelectorAll('.search__item').length
      const footer = panel.querySelector('.search__footer')?.textContent ?? ''
      const indexing = panel.querySelector('.search__indexing') !== null
      const empty = panel.querySelector('.search__empty')?.textContent ?? ''
      return items > 0 || indexing || (empty !== '' && !empty.includes('搜索中'))
    })()`,
    30000,
  )
  const snapshot = await ev(`(() => {
    const panel = document.querySelector('.search-panel')
    return {
      open: panel !== null,
      items: panel === null ? 0 : panel.querySelectorAll('.search__item').length,
      indexing: panel === null ? null : panel.querySelector('.search__indexing')?.textContent ?? null,
      empty: panel === null ? null : panel.querySelector('.search__empty')?.textContent ?? null,
      footer: panel === null ? null : panel.querySelector('.search__footer')?.textContent ?? '',
      errorBanner: document.querySelector('.banner--error')?.textContent ?? null,
    }
  })()`)
  const errorsAfter = consoleErrors.length

  check('7a 重扫期间搜索面板能打开', opened, snapshot)
  // 判据的字面要求是「显示 building」：重扫会让索引失效，而 `vault_search`
  // 读的是**同一时点**的索引状态（`state_for`），所以它会带着 `building`
  // 回来，面板顶部那句「索引正在建立（N/M）」就该在。夹具 2000 篇的同步
  // 窗口（≈0.8 s）比「点完按钮 → 搜索发出」这段（≈0.15 s）宽得多。
  check(
    '7b 显示「索引正在建立（N/M）」，且不报错',
    snapshot.open &&
      snapshot.errorBanner === null &&
      snapshot.indexing !== null &&
      /索引正在建立（\d+\/\d+）/.test(snapshot.indexing),
    snapshot,
  )
  check(
    '7c 面板可用：有命中或诚实的空态，没有「失败」字样',
    snapshot.open && snapshot.items > 0 && !(snapshot.empty ?? '').includes('失败'),
    { items: snapshot.items, empty: snapshot.empty },
  )
  check('7d 控制台无新报错', errorsAfter === errorsBefore, {
    before: errorsBefore,
    after: errorsAfter,
    last: consoleErrors.slice(-3),
  })

  await ESC()
  await waitFor(`document.querySelector('.search-panel') === null`, 2000)
  await waitFor(`document.querySelector('.rescan-btn')?.disabled === false`, 120000)
}

/* ═══════════ 判据 8：失败可见，旧树不被清空 ═══════════ */

{
  const rowsBefore = await treeRowCount()
  const titlesBefore = await treeTitles()
  const gone = `${VAULT}-gone-${STAMP}`
  let mechanism = 'rename'
  try {
    fs.renameSync(VAULT, gone)
  } catch (err) {
    // Windows 上库目录里有应用正开着的 `index.sqlite`，rename 可能被拒
    // （共享冲突）。退化成「拿走读权限」——对扫描器是同一件事：读不了。
    mechanism = 'icacls-deny'
    log(`  rename 失败（${err.code}），退化成 icacls deny`)
    execFileSync('icacls', [VAULT, '/deny', `${process.env.USERNAME}:(OI)(CI)(RX)`], { stdio: 'ignore' })
  }

  try {
    await ev(`document.querySelector('.rescan-btn').click(); true`)
    await waitFor(
      `(() => { const b = document.querySelector('.rescan-btn'); return b !== null && b.disabled === false && b.textContent.includes('重新扫描') })()`,
      120000,
    )
    await sleep(300)
    const state = await ev(`(() => ({
      error: document.querySelector('.banner--error')?.textContent ?? null,
      rows: document.querySelectorAll('.tree-row').length,
      titles: [...document.querySelectorAll('.tree-row')].map((el) => el.title),
    }))()`)
    check('8a 失败有可见横幅', state.error !== null && state.error.length > 0, {
      mechanism,
      error: state.error,
    })
    check('8b 旧树没被清空（行数与标题都不变）', state.rows === rowsBefore && state.titles.join('|') === titlesBefore.join('|'), {
      before: rowsBefore,
      after: state.rows,
    })
  } finally {
    if (mechanism === 'rename') fs.renameSync(gone, VAULT)
    else execFileSync('icacls', [VAULT, '/remove:d', process.env.USERNAME], { stdio: 'ignore' })
  }

  // 恢复之后再点一次：错误横幅消失，树照常回来（「失败不是终态」）。
  const recovered = await rescan()
  check('8c 库回来后重扫恢复正常，错误横幅消失', recovered.ok && (await ev(`document.querySelector('.banner--error') === null`)), {
    state: recovered.state,
    error: await ev(`document.querySelector('.banner--error')?.textContent ?? null`),
    rows: await treeRowCount(),
  })
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

fs.writeFileSync(
  path.join(OUT_DIR, `rescan-acceptance-${STAMP}.json`),
  JSON.stringify({ results, consoleErrors, stamp: STAMP }, null, 2),
)
app.close()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
