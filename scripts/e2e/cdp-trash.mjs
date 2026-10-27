// M5 第一刀 · 应用内回收站的端到端验收。真实 WebView2 + CDP。
//
// 覆盖交付验收 1–6：
//   1. 删 a/b/c.md → 列表里出现**原路径**（不是只有文件名）
//   2. 恢复回原目录、sha 一致、树里出现、Cmd/Ctrl+Shift+F 能搜到
//   3. 恢复撞名不覆盖，给出新名字，两条都在
//   4. 删完立刻硬杀（taskkill /F，等价 kill -9）重启后仍能看到
//   5. 清单写坏不崩，降级显示「原路径未知」，恢复到库根并提示
//   6. 清空 → 回收目录空、空态文案正确、侧栏条数消失
//
// 验收 7（变异验证：清单写入改回 `fs::write` → 第 4 条必须变红）在 Rust 层做，
// 见 ADR-008 的实测表：正常 `torn = 0`，变异后 `torn = 209/573`；
// 硬杀用例正常 12/12 轮通过，同一变异第 1 轮就抓到 0 字节记录。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`：vite 在 127.0.0.1:1420。脚本会取 `/src/App.tsx` 核对它
//      真的在转译模块（B-18：磁盘上是新的、下发的是旧的）
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16：调试槽位是
//      进程级静态，同进程里跑第二个脚本会让探针重复注入）
//      也可以 `pnpm e2e:trash --restart`，让脚本自己杀掉并重启
//   4. 运行期间源码冻结
//
// 夹具由脚本自己生成（`os.tmpdir()/knowisle-e2e-trash`，每次重建，B-32）。
// 建夹具时应用处于「没有库」的状态：否则 1.5 秒后的自动保存会把刚删掉的文件
// 写回来，红在一个完全正确的实现上。
//
// 跑法：`pnpm e2e:trash`（或 `pnpm e2e:trash --restart`）
// 退出码只有三种（B-19）：0 全跑全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'
import crypto from 'node:crypto'

import { openApp, killApp, waitPortClosed, MOD } from './cdp.mjs'
import { OUT_DIR, TRASH_VAULT as VAULT, sleep } from './env.mjs'
import { resetVault } from './fixtures.mjs'

const TRASH = path.join(VAULT, '.knowisle', 'trash')
const MANIFEST = path.join(VAULT, '.knowisle', 'trash-manifest')
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

/* ───────────────────────── 夹具 ───────────────────────── */

const WORD_C = `深路径唯一词-${STAMP}`
const WORD_D = `降级测试词-${STAMP}`
const WORD_E = `硬杀测试词-${STAMP}`
const WORD_F = `清空测试词-${STAMP}`

const sha = (rel) => {
  const file = path.join(VAULT, rel)
  if (!fs.existsSync(file)) return null
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')
}
const shaOf = (abs) =>
  fs.existsSync(abs) ? crypto.createHash('sha256').update(fs.readFileSync(abs)).digest('hex') : null

/** 回收目录里的条目（按名字排序）。只读文件名与大小，不碰内容。 */
const trashList = () => (fs.existsSync(TRASH) ? fs.readdirSync(TRASH).sort() : [])
/** 清单里的记录（`<entry>.json`）。 */
const manifestList = () => (fs.existsSync(MANIFEST) ? fs.readdirSync(MANIFEST).sort() : [])
/** 回收目录里以 `-<原名>` 结尾的那一条。 */
const trashFor = (rel) => {
  const base = rel.split('/').pop()
  return trashList().filter((name) => name.endsWith(`-${base}`))
}

/**
 * 造夹具：幂等（B-32），且**不删 `.knowisle/`**（应用开着时索引库被占用，
 * Windows 上整目录删会 EBUSY）。回收目录与清单要清：它们的残留会让
 * 「条数」与「撞名落点」这两类断言漂移。
 */
const buildFixture = () =>
  resetVault(
    VAULT,
    {
      'a/b/c.md': `# c\n\n${WORD_C}\n`,
      'nested/d.md': `# d\n\n${WORD_D}\n`,
      'e.md': `# e\n\n${WORD_E}\n`,
      'f.md': `# f\n\n${WORD_F}\n`,
    },
    { clearTrash: true },
  )

/* ───────────────────────────── 会话 ───────────────────────────── */

const consoleErrors = []
let app
let send
let ev
let waitFor
let press
let clickAt
let clickSelector

const bind = (session) => {
  app = session
  send = session.send
  ev = session.ev
  waitFor = session.waitFor
  press = session.press
  clickAt = session.clickAt
  clickSelector = session.clickSelector
}

bind(await openApp({ vault: VAULT, prepare: buildFixture, errors: consoleErrors }))
log(`树已渲染，stats = ${await ev(`document.querySelector('.stats')?.textContent ?? null`)}`)

/* ───────────────────── 输入合成 ───────────────────── */

const ESC = () => press('Escape', 'Escape', 27)
const CTRL_SHIFT_F = () => press('f', 'KeyF', 70, MOD.ctrl | MOD.shift)

/* ───────────────────────── 树 / 面板工具 ───────────────────────── */

const nodeRect = (title) =>
  ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    if (!el) return null
    el.scrollIntoView({ block: 'center' })
    const r = el.getBoundingClientRect()
    return { x: r.left + 30, y: r.top + r.height / 2 }
  })()`)

/** 展开一个折叠的目录（折叠目录的子节点根本不在 DOM 里）。 */
const expandDir = async (title) => {
  const state = await ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    return el === null ? 'missing' : el.getAttribute('aria-expanded')
  })()`)
  if (state === 'missing') return false
  if (state === 'true') return true
  const rect = await nodeRect(title)
  if (rect === null) return false
  await clickAt(rect.x, rect.y)
  return waitFor(
    `(() => {
      const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
      return el !== null && el.getAttribute('aria-expanded') === 'true'
    })()`,
    2000,
  )
}

/**
 * 一步完成「右键 → 删除 → 点确认」，全在一个 evaluate 里。
 *
 * 不用 CDP 鼠标事件走这一步：这里要验的是回收站，不是右键菜单本身
 * （M2 已经验过）。合成 `contextmenu` 派发到真实节点上，走的仍是应用
 * 自己的处理函数——不是桩（B-15 禁止的是替换 `__TAURI_INTERNALS__`）。
 */
const fastDelete = (title) =>
  ev(`(async () => {
    const gap = () => new Promise((r) => setTimeout(r, 0))
    const row = [...document.querySelectorAll('.tree-row--file')].find((el) => el.title === ${JSON.stringify(title)})
    if (!row) return { ok: false, reason: '树里没有这一行' }
    const r = row.getBoundingClientRect()
    row.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.left + 30, clientY: r.top + r.height / 2 }))
    await gap()
    const item = [...document.querySelectorAll('.context-menu__item')].find((el) => el.textContent.trim() === '删除')
    if (!item) return { ok: false, reason: '菜单里没有删除' }
    item.click()
    await gap()
    const btn = document.querySelector('.confirm .btn--danger')
    if (!btn) return { ok: false, reason: '没有确认框' }
    btn.click()
    return { ok: true }
  })()`)
const waitConfirmClosed = () => waitFor(`document.querySelector('.confirm') === null`, 15000)

/**
 * 关掉回收站面板。
 *
 * 恢复之后**面板不关**（M5 第三刀：从回收站里挑东西的人多半还要接着挑）。
 * 焦点此时已经跟着 `openNote` 回到编辑器，所以 Esc 不会送到面板的
 * `onKeyDown` 上——先把焦点还给面板（`tabIndex={-1}`），再按 Esc。
 */
const closeTrash = async () => {
  if (!(await ev(`document.querySelector('.trash-panel') !== null`))) return true
  await ev(`document.querySelector('.trash-panel')?.focus(); true`)
  await ESC()
  return waitFor(`document.querySelector('.trash-panel') === null`, 3000)
}

/**
 * 打开回收站面板，**保证列表是这一秒的事实**。
 *
 * `.trash-link` 是开关：面板已经开着时再点会把它关掉。而「删除一条之后再
 * 打开」必须重新读一次列表（应用不会把删除推给面板），所以这里先关再开，
 * 而不是「已经开着就直接返回」——后者会拿着一份过期列表去点恢复。
 */
const openTrash = async () => {
  if (!(await closeTrash())) return { ok: false, reason: '关不掉已打开的面板' }
  if (!(await clickSelector('.trash-link'))) return { ok: false, reason: '侧栏底部没有回收站入口' }
  const opened = await waitFor(`document.querySelector('.trash-panel') !== null`, 5000)
  if (!opened) return { ok: false, reason: '面板没出现' }
  // 等列表**真的刷新完**：`aria-busy` 由 `loading` 驱动，不靠猜时间。
  await waitFor(`document.querySelector('.trash-panel[aria-busy="true"]') === null`, 10000)
  return { ok: true }
}

/** 面板快照：标题、条数、逐行内容、空态文案、焦点、清空按钮状态。 */
const panelState = () =>
  ev(`(() => {
    const p = document.querySelector('.trash-panel')
    if (p === null) return { open: false }
    const items = [...p.querySelectorAll('.trash__item')].map((li) => {
      const origin = li.querySelector('.trash__origin')
      return {
        name: li.querySelector('.trash__name')?.textContent ?? '',
        origin: origin?.textContent ?? '',
        originUnknown: origin?.classList.contains('is-unknown') ?? false,
        meta: li.querySelector('.trash__meta')?.textContent ?? '',
        button: li.querySelector('.btn')?.textContent ?? '',
      }
    })
    const purge = [...p.querySelectorAll('button')].find((b) => b.textContent.trim() === '清空')
    return {
      open: true,
      title: p.querySelector('.trash__title')?.textContent ?? '',
      count: p.querySelector('.trash__count')?.textContent ?? null,
      items,
      empty: p.querySelector('.trash__empty')?.textContent ?? null,
      error: p.querySelector('.trash__error')?.textContent ?? null,
      notice: p.querySelector('.trash__notice')?.textContent ?? null,
      focusInPanel: p.contains(document.activeElement),
      purgeDisabled: purge === undefined ? null : purge.disabled,
    }
  })()`)

/** 恢复回执。它写在面板的 `notice` 行里——顶栏 flash 会被面板盖住。 */
const noticeText = () => ev(`document.querySelector('.trash__notice')?.textContent ?? null`)

/**
 * 点第 n 行的「恢复」，等**那一行从列表里消失**。
 *
 * 判据不能是「面板关掉」：M5 第三刀起面板刻意不关。用行数变化也不赌时间——
 * 恢复是异步的（搬文件 + 重扫 + 打开），行消失才是用户看到的事实。
 */
const clickRestore = async (index = 0) => {
  const before = await ev(`document.querySelectorAll('.trash__item').length`)
  const rect = await ev(`(() => {
    const li = [...document.querySelectorAll('.trash__item')][${index}]
    if (!li) return null
    const btn = [...li.querySelectorAll('button')].find((b) => b.textContent.trim() === '恢复')
    if (!btn) return null
    const r = btn.getBoundingClientRect()
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
  })()`)
  if (rect === null) return false
  await clickAt(rect.x, rect.y)
  const removed = await waitFor(`document.querySelectorAll('.trash__item').length < ${before}`, 20000)
  await sleep(400)
  return removed
}

/** 全文搜索：Ctrl+Shift+F → 真鼠标点输入框 → 输入 → 等「结果那一版」（B-22）。 */
const search = async (needle) => {
  await CTRL_SHIFT_F()
  if (!(await waitFor(`document.querySelector('.search__input') !== null`, 5000))) return null
  if (!(await clickSelector('.search__input'))) return null
  await sleep(80)
  await send('Input.insertText', { text: needle })
  await waitFor(`(document.querySelector('.search__footer')?.textContent ?? '').includes('建索引')`, 30000)
  const snapshot = await ev(`(() => ({
    footer: document.querySelector('.search__footer')?.textContent ?? '',
    paths: [...document.querySelectorAll('.search__path')].map((e) => e.textContent),
  }))()`)
  await ESC()
  await waitFor(`document.querySelector('.search__input') === null`, 3000)
  return snapshot
}

/* ───────────────────────── 准备 ───────────────────────── */

if (!(await expandDir('a'))) throw new Error('展不开 a')
if (!(await expandDir('a/b'))) throw new Error('展不开 a/b')
if (!(await expandDir('nested'))) throw new Error('展不开 nested')

const countBadge = () => ev(`document.querySelector('.trash-link__count')?.textContent ?? null`)

/* ───────────── 验收 1：删除 → 列表里出现原路径 ───────────── */

{
  const shaBefore = sha('a/b/c.md')
  await fastDelete('a/b/c.md')
  const closed = await waitConfirmClosed()
  await sleep(250)

  const entries = trashFor('a/b/c.md')
  const records = manifestList()
  check('1a 文件进了回收目录，清单写了一条', closed && entries.length === 1 && records.length === 1, {
    closed,
    trash: trashList(),
    manifest: records,
    shaBefore,
  })
  const badge = await countBadge()
  check('1b 侧栏底部条数为 1', badge === '1', { badge })

  const opened = await openTrash()
  const state = await panelState()
  const row = state.items?.[0] ?? {}
  check('1c 列表显示原名与原路径', opened.ok && row.name === 'c.md' && row.origin === 'a/b/c.md', {
    row,
    items: state.items,
  })
  check(
    '1d 大小与删除时间都可读',
    /^\d+(\.\d+)? (B|KB|MB|GB)$/.test((row.meta ?? '').split(' · ')[0]) &&
      /(\d{4}\/)?\d{1,2}\/\d{1,2} \d{2}:\d{2}/.test((row.meta ?? '').split(' · ')[1] ?? ''),
    { meta: row.meta },
  )
  check('1e 面板打开后焦点在面板内（Esc 才关得掉）', state.focusInPanel === true, {
    focusInPanel: state.focusInPanel,
  })
  check('1f 清空按钮可用', state.purgeDisabled === false, { purgeDisabled: state.purgeDisabled })
}

/* ───────────── 验收 2：恢复回原目录 + 树 + 搜索 ───────────── */

{
  const shaBefore = shaOf(path.join(TRASH, trashFor('a/b/c.md')[0] ?? 'x'))
  const restored = await clickRestore(0)
  const notice = await noticeText()
  const stillOpen = await ev(`document.querySelector('.trash-panel') !== null`)

  check('2a 恢复到 a/b/c.md，内容一字不差，面板不关', restored && stillOpen && sha('a/b/c.md') === shaBefore, {
    restored,
    stillOpen,
    shaBefore,
    shaNow: sha('a/b/c.md'),
    exists: fs.existsSync(path.join(VAULT, 'a/b/c.md')),
  })
  check('2b 清单记录已被删掉（不留孤儿）', manifestList().length === 0, { manifest: manifestList() })
  check('2c notice 说清了落点', notice === '已恢复到 a/b/c.md', { notice })

  // 面板还开着，树在它后面。展开要真实鼠标点击，所以先关掉面板。
  await closeTrash()

  // 树：恢复之后必须自己出现在树里（vaultScan + revealPath），不靠手动刷新。
  if (!(await expandDir('a'))) throw new Error('恢复后展不开 a')
  if (!(await expandDir('a/b'))) throw new Error('恢复后展不开 a/b')
  const titles = await ev(`[...document.querySelectorAll('.tree-row')].map((e) => e.title)`)
  check('2d 树里重新出现 a/b/c.md', titles.includes('a/b/c.md'), {
    hit: titles.filter((t) => t.includes('c.md')),
  })

  // 搜索：恢复后索引必须知道它回来了。**不重启、不手动重扫**。
  const hit = await search(WORD_C)
  check('2e Cmd/Ctrl+Shift+F 能搜到恢复的笔记', hit !== null && hit.paths.includes('a/b/c.md'), hit)

  check('2f 侧栏条数回到空（不显示 0）', (await countBadge()) === null, { badge: await countBadge() })
}

/* ───────────── 验收 3：恢复撞名不覆盖 ───────────── */

{
  // 删掉刚恢复的那篇，然后在同一个路径上手工写一份**不同内容**的文件。
  await fastDelete('a/b/c.md')
  await waitConfirmClosed()
  await sleep(200)
  const shaTrashed = shaOf(path.join(TRASH, trashFor('a/b/c.md')[0] ?? 'x'))
  fs.mkdirSync(path.join(VAULT, 'a', 'b'), { recursive: true })
  fs.writeFileSync(path.join(VAULT, 'a', 'b', 'c.md'), `# 我后来写的\n\n占位-${STAMP}\n`)
  const shaOccupied = sha('a/b/c.md')

  const opened = await openTrash()
  const restored = await clickRestore(0)
  const notice = await noticeText()

  check(
    '3a 撞名时给出新名字（a/b/c 2.md），不覆盖',
    opened.ok && restored && fs.existsSync(path.join(VAULT, 'a/b/c 2.md')),
    { restored, files: fs.readdirSync(path.join(VAULT, 'a', 'b')).sort(), notice },
  )
  check(
    '3b 两条都在，内容各自对得上',
    sha('a/b/c.md') === shaOccupied && sha('a/b/c 2.md') === shaTrashed,
    { occupied: [shaOccupied, sha('a/b/c.md')], restored: [shaTrashed, sha('a/b/c 2.md')] },
  )
  check(
    '3c notice 明说改了名字',
    (notice ?? '').includes('已恢复为 a/b/c 2.md') && (notice ?? '').includes('原名已被占用'),
    { notice },
  )

  await closeTrash()
  if (!(await expandDir('a'))) throw new Error('撞名恢复后展不开 a')
  if (!(await expandDir('a/b'))) throw new Error('撞名恢复后展不开 a/b')
  const titles = await ev(`[...document.querySelectorAll('.tree-row')].map((e) => e.title)`)
  check(
    '3d 树里两行都在',
    titles.includes('a/b/c.md') && titles.includes('a/b/c 2.md'),
    { rows: titles.filter((t) => t.includes('/c')) },
  )
}

/* ───────────── 验收 5：清单写坏 → 降级，不崩 ───────────── */

{
  const errorsBefore = consoleErrors.length
  await fastDelete('nested/d.md')
  await waitConfirmClosed()
  await sleep(200)

  const entry = trashFor('nested/d.md')[0] ?? null
  const recordPath = entry === null ? null : path.join(MANIFEST, `${entry}.json`)
  // 半截 JSON：模拟"写到一半断电"（A5 没生效时的形态）。
  if (recordPath !== null) fs.writeFileSync(recordPath, '{"origin":"nested/d.md"')

  const opened = await openTrash()
  const state = await panelState()
  const row = (state.items ?? []).find((it) => it.name === 'd.md') ?? {}
  check(
    '5a 清单坏了列表照常显示，只是原路径未知',
    opened.ok && row.name === 'd.md' && row.origin === '原路径未知（清单缺失）' && row.originUnknown === true,
    { row, items: state.items, record: recordPath },
  )

  // sha 必须在恢复**之前**取：恢复就是把它从回收目录搬走，之后那儿只剩 null。
  const shaTrashed = shaOf(path.join(TRASH, entry ?? 'x'))
  const restored = await clickRestore(0)
  const notice = await noticeText()
  check(
    '5b 降级恢复到库根，文件没丢',
    restored && sha('d.md') !== null && sha('d.md') === shaTrashed,
    { restored, shaTrashed, root: sha('d.md'), nested: sha('nested/d.md'), notice },
  )
  check(
    '5c notice 说清"落到库根"和"记录损坏"',
    (notice ?? '').includes('已恢复到库根') && (notice ?? '').includes('清单记录损坏'),
    { notice },
  )
  check('5d 坏清单没有引发任何报错', consoleErrors.length === errorsBefore, {
    before: errorsBefore,
    after: consoleErrors.length,
    last: consoleErrors.slice(-2),
  })
}

/* ───────────── 验收 4：删完立刻硬杀，重启后仍在 ───────────── */

{
  await closeTrash()
  await fastDelete('e.md')
  const closed = await waitConfirmClosed()
  // 不再等任何东西：确认框一关（IPC 已返回）就硬杀。这一秒里唯一可能出问题的
  // 是清单文件，而它必须是"要么没有、要么完整"，不能是半截。
  killApp()
  await waitPortClosed()
  check('4a 删除已完成（确认框已关）后立刻硬杀', closed === true, { closed })

  bind(await openApp({ vault: VAULT, restart: true, errors: consoleErrors }))
  if (!(await expandDir('a'))) {
    // 重启后目录默认折叠，展不开不影响后面（只用到回收站面板）。
    log('  （a 展开失败，不影响本条）')
  }
  const opened = await openTrash()
  const state = await panelState()
  const row = (state.items ?? []).find((it) => it.name === 'e.md') ?? {}
  check(
    '4b 硬杀重启后条目仍在，原路径完整',
    opened.ok && row.name === 'e.md' && row.origin === 'e.md',
    { items: state.items, manifest: manifestList() },
  )

  const restored = await clickRestore(0)
  await sleep(400)
  check('4c 跨进程恢复也能回到原路径', restored && sha('e.md') !== null, { restored })
}

/* ───────────── 验收 6：清空 ───────────── */

{
  // 还剩一条：上一轮撞名恢复出来的 c 2.md 不在了，但 d.md 已恢复、
  // e.md 已恢复。这里再删一条确保回收站非空。
  await closeTrash()
  await fastDelete('f.md')
  await waitConfirmClosed()
  await sleep(200)

  const opened = await openTrash()
  const before = await panelState()
  const openedDialog = await clickSelector('.trash-panel .btn--danger')
  const dialog = await ev(`(() => {
    const d = document.querySelector('.confirm')
    if (d === null) return { open: false }
    return {
      open: true,
      title: d.querySelector('.confirm__title')?.textContent ?? '',
      body: d.querySelector('.confirm__body')?.textContent ?? '',
      dangerLabel: d.querySelector('.btn--danger')?.textContent ?? '',
    }
  })()`)
  check(
    '6a 清空要二次确认，且说清是永久删除',
    opened.ok && openedDialog && dialog.open && dialog.title === '清空回收站？' && dialog.dangerLabel === '永久删除',
    dialog,
  )

  const rect = await ev(`(() => {
    const btn = document.querySelector('.confirm .btn--danger')
    if (btn === null) return null
    const r = btn.getBoundingClientRect()
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
  })()`)
  if (rect !== null) await clickAt(rect.x, rect.y)
  const purged = await waitFor(`document.querySelector('.confirm') === null`, 15000)
  await waitFor(`document.querySelector('.trash-panel[aria-busy="true"]') === null`, 10000)
  await sleep(200)

  const after = await panelState()
  check('6b 回收目录与清单都空了', purged && trashList().length === 0 && manifestList().length === 0, {
    purged,
    trash: trashList(),
    manifest: manifestList(),
    before: before.count,
  })
  check(
    '6c 空态文案正确（不是「读取中…」）',
    after.empty === '回收站是空的。删除的笔记会先放在这里，可以随时恢复。',
    { empty: after.empty },
  )
  check('6d 侧栏条数消失', (await countBadge()) === null, { badge: await countBadge() })
  check('6e 清空按钮变灰', after.purgeDisabled === true, { purgeDisabled: after.purgeDisabled })

  // Esc 关掉面板，回到没有浮层的状态。
  const closedPanel = await closeTrash()
  check('6f Esc 能关掉面板', closedPanel === true, { closedPanel })
}

/* ───────────────────────────── 汇总 ───────────────────────────── */

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
log(
  `\n${results.length} 项：通过 ${results.length - failed.length - skipped.length}，失败 ${failed.length}，跳过 ${skipped.length}`,
)
if (skipped.length > 0) log(`⚠ ${skipped.length} 项没有执行，本次验收不完整，不能当全绿看`)
if (consoleErrors.length > 0) log(`页面报错 ${consoleErrors.length} 条：\n${consoleErrors.join('\n')}`)

fs.writeFileSync(
  path.join(OUT_DIR, `trash-acceptance-${STAMP}.json`),
  JSON.stringify({ results, consoleErrors, stamp: STAMP }, null, 2),
)

app.close()
killApp()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
