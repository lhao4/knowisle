// M5 第二刀 · 回收站的「降级判据」与「已经发生了什么」的端到端验收。
// 真实 WebView2 + CDP。覆盖交付验收 1–5、7；第 6 条显式 SKIP。
//
//   1. 清单记录被删 → 落库根，文案说「没有这条记录」
//   2. 清单写成半截 → 落库根，文案说「清单记录损坏」，且与第 1 条**措辞不同**
//   3. 删完再把原目录删掉 → 落库根，文案说「原目录 nested 已不存在」
//   4. 原路径上放一个同名**目录** → 恢复成 `a/b/f 2.md`，不落库根，文案说「原名已被占用」
//   5. 恢复搬不动 → 命令报 IO，条目留在回收站，面板错误行可读，没有文件被搬到库根
//   7. 清空时有一篇被占用 → 确认框关闭、列表仍列出那 1 条、错误行说「已永久删除 2 篇，1 篇删不掉」
//
// 第 6 条（索引库不可用时 `indexed === false`）**在端到端里造不出来**：
// 要让索引库整体不可用才能复现，而那条路会先让别的路径坏掉（`vault_scan`
// 的同步、搜索、恢复后的重扫都会先报错），分不清是"索引坏了"还是"应用坏了"。
// 它由 `ki-ipc/tests/trash.rs` 的 `a_restore_that_cannot_update_the_index_still_succeeds`
// 覆盖（直接删掉索引库的 `meta` 表，让 `reindex` 在 SQL 层失败）。
//
// 第 5 条在 Windows 上用的是**独占句柄占住回收目录里的那个文件**（共享冲突），
// 不是 Unix 的"父目录 555"——Windows 的目录只读属性不阻止 rename。两者验的
// 是同一件事：搬不动时命令报错、条目还在、没有文件被悄悄放到库根。
// 只读父目录那一支是 `#[cfg(unix)]`，在 CI 上跑。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`：vite 在 127.0.0.1:1420（脚本会取 /src/App.tsx 核对转译结果，B-18）
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16：调试槽位是进程级静态）
//      或 `pnpm e2e:trash-restore --restart` 让脚本自己杀掉并重启
//   4. 运行期间源码冻结
//
// 夹具：`os.tmpdir()/knowisle-e2e-trash-restore`，每次重建（B-32）。
// 跑法：`pnpm e2e:trash-restore`（或加 `--restart`）
// 退出码只有三种（B-19）：0 全跑全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'
import { spawn } from 'node:child_process'

import { openApp, killApp, MOD } from './cdp.mjs'
import { OUT_DIR, TRASH_RESTORE_VAULT as VAULT, sleep } from './env.mjs'
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

const WORD_C = `记录缺失词-${STAMP}`
const WORD_D = `记录损坏词-${STAMP}`
const WORD_E = `目录没了词-${STAMP}`
const WORD_F = `同名目录词-${STAMP}`
const WORD_G = `占用词-${STAMP}`
const WORD_H = `清空甲-${STAMP}`
const WORD_I = `清空乙-${STAMP}`

const trashList = () => (fs.existsSync(TRASH) ? fs.readdirSync(TRASH).sort() : [])
const manifestList = () => (fs.existsSync(MANIFEST) ? fs.readdirSync(MANIFEST).sort() : [])
const trashFor = (rel) => {
  const base = rel.split('/').pop()
  return trashList().filter((name) => name.endsWith(`-${base}`))
}

/**
 * 七篇笔记一次性造好：应用没有文件监听（B-09），中途新写进磁盘的文件要等
 * 下一次扫描才可见，而"下一次扫描"只由删除/恢复/新建触发。
 *
 * 幂等靠 `resetVault`（B-32）：上一轮恢复出来的 `c.md`、`f 2.md` 会改变这一轮
 * 的落点，红得像是实现坏了。
 */
const buildFixture = () =>
  resetVault(
    VAULT,
    {
      'a/b/c.md': `# c\n\n${WORD_C}\n`,
      'nested/d.md': `# d\n\n${WORD_D}\n`,
      'nested/e.md': `# e\n\n${WORD_E}\n`,
      'a/b/f.md': `# f\n\n${WORD_F}\n`,
      'g.md': `# g\n\n${WORD_G}\n`,
      'h.md': `# h\n\n${WORD_H}\n`,
      'i.md': `# i\n\n${WORD_I}\n`,
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

const ESC = () => press('Escape', 'Escape', 27)

/* ───────────────────────── 树 / 面板工具 ───────────────────────── */

const nodeRect = (title) =>
  ev(`(() => {
    const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
    if (!el) return null
    el.scrollIntoView({ block: 'center' })
    const r = el.getBoundingClientRect()
    return { x: r.left + 30, y: r.top + r.height / 2 }
  })()`)

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
 * 这里要验的是回收站，不是右键菜单本身（M2 已经验过）。合成 `contextmenu`
 * 派发到真实节点上，走的仍是应用自己的处理函数——不是桩。
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

/** 删除一篇并等到磁盘上真的出现那条回收记录（结果落地，不猜时间）。 */
const deleteNote = async (title) => {
  const outcome = await fastDelete(title)
  const closed = await waitConfirmClosed()
  const landed = closed && trashFor(title).length > 0
  return { ...outcome, closed, landed, entries: trashFor(title) }
}

/**
 * 关掉回收站面板。
 *
 * 恢复之后面板不关（M5 第三刀），焦点也可能已经跟着 `openNote` 回到编辑器，
 * 所以 Esc 不一定送得到面板的 `onKeyDown` 上——先把焦点还给面板再按。
 */
const closeTrash = async () => {
  if (!(await ev(`document.querySelector('.trash-panel') !== null`))) return true
  await ev(`document.querySelector('.trash-panel')?.focus(); true`)
  await ESC()
  return waitFor(`document.querySelector('.trash-panel') === null`, 3000)
}

/**
 * 打开回收站面板，**每次都重新读一遍列表**。
 *
 * 这个脚本会从外面改磁盘（删清单、写半截 JSON、删原目录、放同名目录），
 * 应用不知道这些改动。`.trash-link` 是开关，所以「先关再开」既能保证面板
 * 开着，又能触发一次 `refreshTrash`——不重新读的话，列表上那条记录的
 * `origin` 还是改坏之前缓存下来的值，断言会红在一个完全正确的实现上。
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

const panelState = () =>
  ev(`(() => {
    const p = document.querySelector('.trash-panel')
    if (p === null) return { open: false }
    const items = [...p.querySelectorAll('.trash__item')].map((li) => ({
      name: li.querySelector('.trash__name')?.textContent ?? '',
      origin: li.querySelector('.trash__origin')?.textContent ?? '',
      originUnknown: li.querySelector('.trash__origin')?.classList.contains('is-unknown') ?? false,
    }))
    return {
      open: true,
      count: p.querySelector('.trash__count')?.textContent ?? null,
      items,
      empty: p.querySelector('.trash__empty')?.textContent ?? null,
      error: p.querySelector('.trash__error')?.textContent ?? null,
      notice: p.querySelector('.trash__notice')?.textContent ?? null,
      focusInPanel: p.contains(document.activeElement),
    }
  })()`)

/** 恢复回执写在面板的 `notice` 行里（顶栏 flash 会被面板盖住）。 */
const noticeText = () => ev(`document.querySelector('.trash__notice')?.textContent ?? null`)

/**
 * 点「名字是 name 的那一行」的恢复按钮，等**那一行消失或错误行出现**。
 *
 * 两件事分开等，但都不再等"面板关掉"：M5 第三刀起面板刻意不关。行消失 =
 * 成功；错误行出现 = 失败（第 5 条）。这两者都是结果，不是时间。
 */
const restoreAndWait = async (name) => {
  const appeared = await waitFor(
    `[...document.querySelectorAll('.trash__item')].some((el) => (el.querySelector('.trash__name')?.textContent ?? '') === ${JSON.stringify(name)})`,
    8000,
  )
  if (!appeared) return { clicked: false, settled: false, notice: null }
  const rect = await ev(`(() => {
    const li = [...document.querySelectorAll('.trash__item')].find((el) => (el.querySelector('.trash__name')?.textContent ?? '') === ${JSON.stringify(name)})
    if (!li) return null
    const btn = [...li.querySelectorAll('button')].find((b) => b.textContent.trim() === '恢复')
    if (!btn) return null
    const r = btn.getBoundingClientRect()
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
  })()`)
  if (rect === null) return { clicked: false, settled: false, notice: null }
  await clickAt(rect.x, rect.y)
  const settled = await waitFor(
    `(() => {
      const row = [...document.querySelectorAll('.trash__item')].some((el) => (el.querySelector('.trash__name')?.textContent ?? '') === ${JSON.stringify(name)})
      if (!row) return true
      const err = document.querySelector('.trash-panel .trash__error')
      return err !== null && (err.textContent ?? '') !== ''
    })()`,
    25000,
  )
  await sleep(300)
  return { clicked: true, settled, notice: await noticeText() }
}

/* ───────────────────── 独占句柄（Windows 的"占用"） ───────────────────── */

/**
 * 用一个真实进程把文件占住（`FileShare.None`），直到 `release()`。
 *
 * Node 的 `fs.open` 默认带共享位，占不住；只有 .NET 的 `FileShare.None`
 * 才会让别的进程连读都打不开——这正是 Windows 上"文件被占用"的形态。
 */
const holdExclusive = async (file) => {
  const escaped = file.replace(/'/g, "''")
  const proc = spawn(
    'powershell',
    [
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      `$f = [System.IO.File]::Open('${escaped}', 'Open', 'Read', 'None'); Start-Sleep -Seconds 300`,
    ],
    { stdio: 'ignore' },
  )

  let locked = false
  for (let i = 0; i < 200; i += 1) {
    try {
      fs.closeSync(fs.openSync(file, 'r'))
      await sleep(50)
    } catch {
      locked = true
      break
    }
  }

  const release = async () => {
    proc.kill()
    for (let i = 0; i < 100; i += 1) {
      try {
        fs.closeSync(fs.openSync(file, 'r'))
        return true
      } catch {
        await sleep(50)
      }
    }
    return false
  }

  return { locked, release }
}

/* ───────────────────────── 准备 ───────────────────────── */

if (!(await expandDir('a'))) throw new Error('展不开 a')
if (!(await expandDir('a/b'))) throw new Error('展不开 a/b')
if (!(await expandDir('nested'))) throw new Error('展不开 nested')

/* ───────── 验收 1：清单记录被删 → 落库根，说「没有这条记录」 ───────── */

let noticeMissing = ''
{
  const del = await deleteNote('a/b/c.md')
  const entry = del.entries[0] ?? null
  if (entry !== null) fs.rmSync(path.join(MANIFEST, `${entry}.json`), { force: true })
  check('1a 删除成功，清单记录被手工删掉', del.landed && entry !== null && !fs.existsSync(path.join(MANIFEST, `${entry}.json`)), {
    entry,
    manifest: manifestList(),
  })

  const opened = await openTrash()
  const before = await panelState()
  const row = (before.items ?? []).find((it) => it.name === 'c.md') ?? {}
  check('1b 列表照常显示，原路径标成未知', opened.ok && row.originUnknown === true, { row })

  const restored = await restoreAndWait('c.md')
  noticeMissing = restored.notice ?? ''
  check(
    '1c 落库根 c.md，notice 说「没有这条记录」',
    restored.settled && fs.existsSync(path.join(VAULT, 'c.md')) &&
      noticeMissing.includes('已恢复到库根 c.md') && noticeMissing.includes('没有这条记录'),
    { restored, root: fs.readdirSync(VAULT).sort() },
  )
}

/* ───────── 验收 2：清单写成半截 → 说「清单记录损坏」，措辞与 1 不同 ───────── */

{
  const del = await deleteNote('nested/d.md')
  const entry = del.entries[0] ?? null
  if (entry !== null) fs.writeFileSync(path.join(MANIFEST, `${entry}.json`), '{"origin":"nested/d.md"')
  check('2a 删除成功，清单被写成半截 JSON', del.landed && entry !== null, { entry })

  const opened = await openTrash()
  const before = await panelState()
  const row = (before.items ?? []).find((it) => it.name === 'd.md') ?? {}
  check('2b 坏记录不崩，列表照常显示', opened.ok && row.name === 'd.md' && row.originUnknown === true, { row })

  const restored = await restoreAndWait('d.md')
  const wordingCorrupt = restored.notice ?? ''
  check(
    '2c 落库根 d.md，notice 说「清单记录损坏」',
    restored.settled && fs.existsSync(path.join(VAULT, 'd.md')) && wordingCorrupt.includes('清单记录损坏'),
    { restored },
  )
  check(
    '2d 与第 1 条的措辞不同（两种事实两句话）',
    wordingCorrupt !== '' && noticeMissing !== '' && wordingCorrupt !== noticeMissing,
    { wordingMissing: noticeMissing, wordingCorrupt },
  )
}

/* ───────── 验收 3：删完再把原目录删掉 → 说「原目录 nested 已不存在」 ───────── */

{
  const del = await deleteNote('nested/e.md')
  // 目录里只剩这一个文件（d.md 已在第 2 条里被删掉），整块删掉才是"外部删除"。
  fs.rmSync(path.join(VAULT, 'nested'), { recursive: true, force: true })
  check('3a 删除成功，原目录 nested 被外部删掉', del.landed && !fs.existsSync(path.join(VAULT, 'nested')), {
    entries: del.entries,
  })

  const opened = await openTrash()
  const before = await panelState()
  const row = (before.items ?? []).find((it) => it.name === 'e.md') ?? {}
  check('3b 记录完好：列表仍显示原路径 nested/e.md', opened.ok && row.origin === 'nested/e.md' && row.originUnknown === false, {
    row,
  })

  const restored = await restoreAndWait('e.md')
  check(
    '3c 落库根 e.md，notice 说「原目录 nested 已不存在」',
    restored.settled && fs.existsSync(path.join(VAULT, 'e.md')) &&
      (restored.notice ?? '').includes('原目录 nested 已不存在'),
    { restored },
  )
  check('3d 不代建原目录', !fs.existsSync(path.join(VAULT, 'nested')), { nested: fs.existsSync(path.join(VAULT, 'nested')) })
}

/* ───────── 验收 4：原路径上是同名目录 → 改名，不落库根 ───────── */

{
  const del = await deleteNote('a/b/f.md')
  // 原路径上放一个同名**目录**：它不是"原目录没了"，是"这个名字被占了"。
  fs.mkdirSync(path.join(VAULT, 'a', 'b', 'f.md'), { recursive: true })
  check('4a 删除成功，原路径上出现同名目录', del.landed && fs.statSync(path.join(VAULT, 'a/b/f.md')).isDirectory(), {
    entries: del.entries,
  })

  const opened = await openTrash()
  const before = await panelState()
  check('4b 面板里能看到 f.md', opened.ok && (before.items ?? []).some((it) => it.name === 'f.md'), {
    items: before.items,
  })

  const restored = await restoreAndWait('f.md')
  check(
    '4c 在原目录里改名为 a/b/f 2.md',
    restored.settled && fs.existsSync(path.join(VAULT, 'a/b/f 2.md')),
    { restored, files: fs.readdirSync(path.join(VAULT, 'a', 'b')).sort() },
  )
  check(
    '4d 不落库根，也不覆盖那个目录',
    !fs.existsSync(path.join(VAULT, 'f.md')) && fs.statSync(path.join(VAULT, 'a/b/f.md')).isDirectory(),
    { root: fs.readdirSync(VAULT).sort() },
  )
  check(
    '4e notice 说「已恢复为 a/b/f 2.md（原名已被占用）」',
    (restored.notice ?? '').includes('已恢复为 a/b/f 2.md') && (restored.notice ?? '').includes('原名已被占用'),
    { notice: restored.notice },
  )
}

/* ───────── 验收 5：搬不动 → 报 IO，条目还在，没有文件被搬到库根 ───────── */

let lockedEntry = null
let releaseLock = async () => false
{
  const del = await deleteNote('g.md')
  const entry = del.entries[0] ?? null
  lockedEntry = entry
  const lock = entry === null ? { locked: false, release: releaseLock } : await holdExclusive(path.join(TRASH, entry))
  releaseLock = lock.release
  check('5a 删除成功，并用独占句柄占住回收目录里的那份', del.landed && lock.locked, {
    entry,
    locked: lock.locked,
  })

  const opened = await openTrash()
  const restored = await restoreAndWait('g.md')
  const after = await panelState()

  check('5b 恢复失败：面板还开着，错误行可读', opened.ok && restored.settled && after.open === true && (after.error ?? '') !== '', {
    restored,
    error: after.error,
  })
  check('5c 没有文件被搬到库根', !fs.existsSync(path.join(VAULT, 'g.md')), { root: fs.readdirSync(VAULT).sort() })
  check('5d 条目仍在回收目录里（还能再试）', entry !== null && fs.existsSync(path.join(TRASH, entry)), {
    trash: trashList(),
  })
  check('5e 失败时不该说「已恢复」', restored.notice === null, { notice: restored.notice })
}

/* ───────── 验收 7：清空时有一篇被占用 ───────── */

{
  // 第 5 条留下的那条 + 两篇新的 = 3 条。删除走树里的右键菜单，不需要面板；
  // 删完再开面板（`openTrash` 幂等，面板这会儿本来就开着）。
  for (const name of ['h.md', 'i.md']) {
    const del = await deleteNote(name)
    if (!del.landed) throw new Error(`删不掉 ${name}`)
  }
  const opened = await openTrash()
  // 等**条数落地**，不等 aria-busy 落空：刷新可能还没开始，`aria-busy` 那时
  // 压根没置上，`=== null` 立刻为真。断言 count 才是那条"结果"。
  const three = await waitFor(`(document.querySelector('.trash__count')?.textContent ?? '') === '3'`, 8000)
  const before = await panelState()
  check('7a 回收站里有 3 条', opened.ok && three && before.count === '3', { count: before.count, items: before.items })

  const openedDialog = await clickSelector('.trash-panel .btn--danger')
  const dialogOpen = await waitFor(`document.querySelector('.confirm .btn--danger') !== null`, 3000)
  const confirmed = dialogOpen ? await clickSelector('.confirm .btn--danger') : false
  const purged = await waitFor(`document.querySelector('.confirm') === null`, 20000)
  // 同样的道理：等条数变成 1，而不是等 `aria-busy` 落空。
  const one = await waitFor(`(document.querySelector('.trash__count')?.textContent ?? '') === '1'`, 8000)
  const after = await panelState()

  check('7b 二次确认打开并点了「永久删除」', openedDialog && dialogOpen && confirmed && purged, {
    openedDialog,
    dialogOpen,
    confirmed,
    purged,
  })
  check(
    '7c 删掉的那 2 篇真的没了，被占用的那篇还在',
    trashList().length === 1 && lockedEntry !== null && fs.existsSync(path.join(TRASH, lockedEntry)),
    { trash: trashList(), lockedEntry },
  )
  check(
    '7d 列表刷新后仍列出那 1 条',
    one && after.open === true && (after.items ?? []).length === 1 && after.count === '1',
    { one, count: after.count, items: after.items },
  )
  check(
    '7e 错误行说清「已永久删除 2 篇，1 篇删不掉」',
    (after.error ?? '').includes('已永久删除 2 篇') && (after.error ?? '').includes('1 篇删不掉'),
    { error: after.error },
  )
  // 清空必须**按条清清单**。整块 `remove_dir_all` 会把删不掉那一篇的记录一起
  // 删掉：文件还在回收目录里，原路径却没了，列表上那一条当场退化成
  // 「原路径未知」——刚说完"删不掉的已留在列表里"，就把它的原路径扔了。
  const survivor = (after.items ?? [])[0] ?? {}
  check('7f 删不掉的那篇仍知道原路径', survivor.origin === 'g.md' && survivor.originUnknown === false, {
    survivor,
    manifest: manifestList(),
  })
}

/* ───────── 验收 6：显式 SKIP ───────── */

skip(
  '6 索引库不可用时恢复仍 Ok、indexed === false',
  '端到端造不出来：要让索引库整体不可用才能复现，而那条路会先让 vault_scan 的同步与重扫报错，分不清是"索引坏了"还是"应用坏了"；由 ki-ipc/tests/trash.rs 覆盖',
)

/* ───────────────────────── 收尾 ───────────────────────── */

// 放开句柄，再确认一遍"放开之后还能恢复"——锁不是把条目弄坏了。
const released = await releaseLock()
if (released && lockedEntry !== null) {
  const opened = await openTrash()
  const restored = await restoreAndWait('g.md')
  // 断言「已恢复到 g.md」而不是「已恢复到库根 g.md」：后者是降级文案，
  // 说明清单记录已经丢了。清空按条清之后这条记录必须还在。
  check(
    '5f 放开占用后还能恢复，且没有降级（记录还在）',
    opened.ok && restored.settled && fs.existsSync(path.join(VAULT, 'g.md')) &&
      (restored.notice ?? '').includes('已恢复到 g.md'),
    { restored },
  )
}

// 面板这会儿还开着；焦点可能已经跟着 openNote 回到编辑器，先把焦点还给面板。
await ev(`document.querySelector('.trash-panel')?.focus(); true`)
await ESC()
await waitFor(`document.querySelector('.trash-panel') === null`, 3000)

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
log(
  `\n${results.length} 项：通过 ${results.length - failed.length - skipped.length}，失败 ${failed.length}，跳过 ${skipped.length}`,
)
if (skipped.length > 0) log(`⚠ ${skipped.length} 项没有执行，本次验收不完整，不能当全绿看`)
if (consoleErrors.length > 0) log(`页面报错 ${consoleErrors.length} 条：\n${consoleErrors.join('\n')}`)

fs.writeFileSync(
  path.join(OUT_DIR, `trash-restore-acceptance-${STAMP}.json`),
  JSON.stringify({ results, consoleErrors, stamp: STAMP }, null, 2),
)

app.close()
killApp()
process.exitCode = failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0
