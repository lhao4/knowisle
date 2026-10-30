// M7 第十二刀（B-04）· 跳过清单的端到端验收。
//
// # 这一条在验什么
//
// 「跳过 3」这句话此前在三个地方意味着三件不同的事，而用户一个也查不了：
// 一个目录没权限、一个文件名是 GBK 编的、一个 FIFO，全记成 1；索引侧一篇
// 认不出编码、一篇超过 2MB、一篇 IO 失败，也全记成 1。两处还显示同一个词，
// 于是「侧栏说跳过 3、搜索说跳过 1」看起来像 bug。
//
// 八条判据（编号对齐施工指令 §4）：
//
//   1. 四类跳过都有的夹具上，侧栏数字 = 这四类之和，清单逐条定位到路径与原因；
//   2. 四类必须显示**四句不同的人话**；
//   3. 同一夹具上扫描侧数字 ≠ 索引侧数字（>2MB 的篇在扫描侧是正常笔记）；
//   4. 把 >2MB 那篇删掉 → 索引侧清单少一条，扫描侧不变；
//   5. 「认不出编码」与「待转存」两个清单**不重叠**；
//   6. 0 条时入口不出现；
//   7. 60 个非 UTF-8 文件名 → `skipped = 60`、清单 50 条、截断提示出现（仅 Unix）；
//   8. 运行期无 console error。
//
// # 为什么 Windows 上会跳过两项
//
// - **非 UTF-8 文件名**：NTFS 的名字是 UTF-16，Rust 的 `to_str()` 只在
//   「不成对的代理项」时返回 None，而那种名字 Win32 根本建不出来。
// - **FIFO**：Windows 没有 `mkfifo`。
//
// 造不出来的类必须 `skip()` 并让退出码为 2（B-19）：静默通过比没验更糟。
//
// 前置（缺一条都不算数）：
//   1. `pnpm --filter @knowisle/desktop dev`（只有 vite，别用 `pnpm dev`）
//   2. `cargo build -p knowisle`（否则测的是旧二进制）
//   3. 应用**刚重启**（`--restart` 自己杀并起）
//
// 跑法：`node scripts/e2e/cdp-skip-list.mjs --restart`
// 退出码三种（B-19）：0 全过 / 1 有失败 / 2 跑过但有跳过。
import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'

import { killApp, openApp } from './cdp.mjs'
import { OUT_DIR, SKIP_VAULT as VAULT, sleep } from './env.mjs'
import { assertTempVault, resetVault } from './fixtures.mjs'

const UNIX = process.platform !== 'win32'
const USER = process.env.USERNAME ?? process.env.USER ?? ''

/** 无读权限的目录。扫描侧计一条 `permission`，索引侧根本看不见它。 */
const LOCKED = 'locked'

/**
 * 超过索引上限（2MB）的笔记：扫描侧是**完全正常的树节点**，只有索引侧跳过。
 *
 * 它就是「两个数不相等」的全部来源——也是这一刀最容易被误合并的地方。
 */
const HUGE = 'bad/huge.md'

/**
 * 认不出编码的笔记。
 *
 * 字节是 `[0x01, 0xFF, 0x02]` 重复 7 次（21 字节，**奇数**）。
 * `0x01` 是判据：任何编码解出来都会带 `U+0001`，`is_plausible` 判它猜错了。
 * 长度必须是奇数——偶数时 `sniff_utf16` 会把 `0x01 0xFF` 读成 `U+FF01`（`！`），
 * 于是它被当成一篇正常笔记索引（第十一刀的 Rust 用例里踩过）。
 */
const WEIRD = 'bad/weird.md'

/** 能解码的 GBK：它进「待转存」清单，**不进**跳过清单（判据 5）。 */
const GBK = 'enc/gbk.md'

/** 非 UTF-8 文件名（仅 Unix）。 */
const BAD_NAME_DIR = 'gbkname'

/** FIFO（仅 Unix）。 */
const FIFO = 'pipe/fifo.md'

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

const abs = (rel) => path.join(VAULT, rel)

/**
 * 给目录加「无读权限」。
 *
 * Windows 只能靠 ACL（`icacls /deny`）；Unix 靠 `chmod 000`。两边都要试，
 * 失败就让调用方 skip——**不许静默放过**。
 */
function lockDir(dir) {
  if (process.platform === 'win32') {
    execFileSync('icacls', [dir, '/deny', `${USER}:(OI)(CI)(RX)`], { stdio: 'ignore' })
    return
  }
  fs.chmodSync(dir, 0o000)
}

/** 还原权限。**必须能还原**：不还原的话下一次 `sweepNotes` 会抛 EACCES。 */
function unlockDir(dir) {
  if (!fs.existsSync(dir)) return
  if (process.platform === 'win32') {
    try {
      execFileSync('icacls', [dir, '/remove:d', USER], { stdio: 'ignore' })
    } catch {
      // 目录可能根本没有 deny 项：`/remove:d` 对不存在的项报错，无害。
    }
    return
  }
  try {
    fs.chmodSync(dir, 0o755)
  } catch {
    // 同上。
  }
}

function unlockAll() {
  unlockDir(abs(LOCKED))
}

/** 60 个非 UTF-8 文件名（判据 7 的夹具）。只在 Unix 上能建。 */
function writeBadNames(dir, count) {
  fs.mkdirSync(dir, { recursive: true })
  for (let index = 0; index < count; index += 1) {
    // GBK 的「中文」= D6 D0 CE C4。Node 收 Buffer 路径，写字符串永远是 UTF-8。
    const bytes = Buffer.concat([
      Buffer.from(dir + '/'),
      Buffer.from([0xd6, 0xd0, 0xce, 0xc4]),
      Buffer.from('-'),
      Buffer.from(String(index).padStart(2, '0')),
      Buffer.from('.md'),
    ])
    fs.writeFileSync(bytes, Buffer.from('# gbk name\n', 'utf8'))
  }
}

/**
 * 建夹具。**四类跳过都要有**（能建的那几类）。
 *
 * `resetVault` 会递归删 `.md`，所以必须先解锁——000 的目录会让它抛 EACCES，
 * 而那个错误看起来像「夹具清理逻辑坏了」。
 */
function buildFixture({ badNames = 0 } = {}) {
  unlockAll()
  resetVault(VAULT, {}, { clearTrash: true })

  fs.mkdirSync(abs('ok'), { recursive: true })
  fs.writeFileSync(abs('ok/a.md'), '# 正常 A\n', 'utf8')
  fs.writeFileSync(abs('ok/b.md'), '# 正常 B\n', 'utf8')

  // 索引侧跳过：超过 2MB。
  fs.mkdirSync(abs('bad'), { recursive: true })
  fs.writeFileSync(abs(HUGE), `huge-only-marker\n${'x'.repeat(2 * 1024 * 1024 + 4096)}\n`, 'utf8')

  // 索引侧跳过：正文认不出编码。**必须用 Buffer**：写字符串永远是 UTF-8。
  const parts = []
  for (let index = 0; index < 7; index += 1) parts.push(Buffer.from([0x01, 0xff, 0x02]))
  fs.writeFileSync(abs(WEIRD), Buffer.concat(parts))

  // 待转存（不是跳过）：能解码的 GBK。
  fs.mkdirSync(abs('enc'), { recursive: true })
  fs.writeFileSync(
    abs(GBK),
    Buffer.from('2320b4fdd7aab4e60ad6d0cec4d5fdcec40a', 'hex'),
  )

  // 扫描侧跳过：无读权限的目录。
  fs.mkdirSync(abs(LOCKED), { recursive: true })
  fs.writeFileSync(abs(`${LOCKED}/inside.md`), '# 读不到的目录里的笔记\n', 'utf8')

  if (UNIX) {
    // 扫描侧跳过：FIFO。
    fs.mkdirSync(abs('pipe'), { recursive: true })
    execFileSync('mkfifo', [abs(FIFO)])
  }
  if (UNIX && badNames > 0) writeBadNames(abs(BAD_NAME_DIR), badNames)

  lockDir(abs(LOCKED))
}

const SCAN_EXPECTED = 1 + (UNIX ? 1 : 0)
const INDEX_EXPECTED = 2

/* ───────────────────────── 会话 ───────────────────────── */

const consoleErrors = []
let app
let ev
let waitFor

assertTempVault(VAULT)
const session = await openApp({
  vault: VAULT,
  prepare: () => buildFixture(),
  errors: consoleErrors,
})
app = session
ev = session.ev
waitFor = session.waitFor
await sleep(400)

/** 侧栏底部那个入口的文本；入口不在时返回 `null`。 */
const sidebarText = () => ev(`document.querySelector('.skip-link')?.textContent ?? null`)

/**
 * 读面板的两栏。
 *
 * 用 `[aria-label]` 定位而不是第 n 个 `<section>`：两栏的顺序是给人看的，
 * 不该成为判据的一部分——调换顺序不该让用例红。
 */
const readPanel = () =>
  ev(`(() => {
    const panel = document.querySelector('.skip-panel')
    if (panel === null) return null
    const section = (label) => {
      const el = panel.querySelector('[aria-label="' + label + '"]')
      if (el === null) return null
      return {
        heading: el.querySelector('.skip__heading')?.textContent ?? '',
        rows: [...el.querySelectorAll('.skip__item')].map((li) => ({
          path: li.querySelector('.encoding__path')?.textContent ?? '',
          flag: li.querySelector('.skip__flag')?.textContent ?? null,
          what: li.querySelector('.skip__what')?.textContent ?? '',
          fix: li.querySelector('.skip__fix')?.textContent ?? '',
        })),
        notices: [...el.querySelectorAll('.trash__notice')].map((n) => n.textContent ?? ''),
        empty: el.querySelector('.trash__empty')?.textContent ?? null,
      }
    }
    return { scan: section('扫描时跳过'), index: section('索引时跳过') }
  })()`)

/** 打开面板（真鼠标点击：顺带验入口没被浮层挡住）。 */
async function openPanel() {
  if (!(await app.clickSelector('.skip-link'))) return null
  if (!(await waitFor(`document.querySelector('.skip-panel') !== null`, 3000))) return null
  return readPanel()
}

/**
 * 等索引侧那一栏满足条件，最多 `timeout` 毫秒。成功时面板**保持打开**。
 *
 * # 为什么不能读一次就断言
 *
 * `reload()` 之后索引的增量同步在后台跑：树先回来，`status = 'skipped'` 的
 * 行后删。第一版只 `sleep(1200)` 就读面板，撞上同步还没提交的那一瞬间会红，
 * 重跑一次又绿——这种检查比没有更糟，它教人「红了就重跑」。
 *
 * 这里改成轮询到条件成立，把「什么时候能看到」变成最终一致性断言。
 * 严格时序（同步在 `applyVault` 之后第几毫秒提交）不归 e2e 管：时序型
 * 端到端测试只证明没有灾难性回归，那部分交给注入时钟的单元测试。
 */
async function waitForIndexPanel(predicate, timeout = 12000) {
  const deadline = Date.now() + timeout
  let last = null
  for (;;) {
    const panel = await openPanel()
    if (panel?.index) {
      last = panel
      if (predicate(panel.index)) return last
    }
    if (Date.now() > deadline) return last
    await closePanel()
    await sleep(400)
  }
}

/** 关掉面板：Esc（面板容器的 onKeyDown 收得到，它挂载时就被聚焦了）。 */
async function closePanel() {
  await app.press('Escape', 'Escape', 27)
  await waitFor(`document.querySelector('.skip-panel') === null`, 2000)
}

/** 重扫：改 `localStorage` 再 reload，让应用走自己的「恢复上次打开的库」。 */
async function reload() {
  await app.reload()
  await waitFor(`document.querySelector('.tree-row') !== null`, 30000)
  // 索引在后台建，入口上的索引侧数字要等它数完（最多 1.5 秒一轮）。
  await sleep(1200)
}

/* ═══════════════════════ 1. 侧栏数字 = 四类之和 ═══════════════════════ */

const entry = await sidebarText()
check('1.sidebar-entry', entry !== null && entry.trim() === `跳过 ${SCAN_EXPECTED}`, {
  entry,
  expected: `跳过 ${SCAN_EXPECTED}`,
  classes: {
    permission: 1,
    fifo: UNIX ? 1 : 0,
    nonUtf8Name: UNIX ? '（下一项单独验）' : 0,
  },
})

const panel = await openPanel()
check('1.panel-opens', panel !== null && panel.scan !== null && panel.index !== null, {
  scan: panel?.scan?.heading ?? null,
  index: panel?.index?.heading ?? null,
})

check(
  '1.scan-count-is-exact',
  panel?.scan?.heading.includes(`${SCAN_EXPECTED} 个条目`) === true,
  { heading: panel?.scan?.heading ?? null },
)
check(
  '1.index-count-is-exact',
  panel?.index?.heading.includes(`${INDEX_EXPECTED} 篇笔记`) === true,
  { heading: panel?.index?.heading ?? null },
)

const scanPaths = (panel?.scan?.rows ?? []).map((row) => row.path)
const indexPaths = (panel?.index?.rows ?? []).map((row) => row.path)
check('1.scan-locked-located', scanPaths.includes(LOCKED), { scanPaths })
check('1.index-huge-located', indexPaths.includes(HUGE), { indexPaths })
check('1.index-weird-located', indexPaths.includes(WEIRD), { indexPaths })

/* ═══════════════════════ 2. 四句不同的人话 ═══════════════════════ */

const allRows = [...(panel?.scan?.rows ?? []), ...(panel?.index?.rows ?? [])]
const whats = allRows.map((row) => row.what)
check('2.reasons-are-distinct', new Set(whats).size === whats.length, {
  count: whats.length,
  distinct: new Set(whats).size,
  whats,
})

const rowOf = (rows, target) => rows.find((row) => row.path === target) ?? null
const lockedRow = rowOf(panel?.scan?.rows ?? [], LOCKED)
const hugeRow = rowOf(panel?.index?.rows ?? [], HUGE)
const weirdRow = rowOf(panel?.index?.rows ?? [], WEIRD)

check('2.permission-copy', lockedRow?.what.includes('没有读权限') === true, lockedRow)
check('2.too-large-copy', hugeRow?.what.includes('2 MB') === true, hugeRow)
check('2.invalid-utf8-copy', weirdRow?.what.includes('编码') === true, weirdRow)

// 每条都要回答「为什么不能自动修」——只写现象等于让用户去找那个不存在的按钮。
check(
  '2.every-row-explains-why',
  allRows.length > 0 && allRows.every((row) => row.fix.length > 0),
  { fixes: allRows.map((row) => row.fix) },
)

/* ═══════════════════════ 3. 两个数不相等 ═══════════════════════ */

check(
  '3.two-numbers-differ',
  SCAN_EXPECTED !== INDEX_EXPECTED && panel?.scan?.heading.includes(`${SCAN_EXPECTED} 个`) === true &&
    panel?.index?.heading.includes(`${INDEX_EXPECTED} 篇`) === true,
  { scan: SCAN_EXPECTED, index: INDEX_EXPECTED },
)

/* ═══════════════════════ 5. 两个清单不重叠 ═══════════════════════ */

// 跳过清单里不许有那篇 GBK（它在待转存清单里）。
check('5.skip-list-excludes-gbk', !indexPaths.includes(GBK), { indexPaths })

await closePanel()

// 待转存入口只在有时才出现；索引建完之后它一定在（GBK 那篇）。
const encodingEntry = await waitFor(`document.querySelector('.encoding-link') !== null`, 20000)
if (encodingEntry) {
  await app.clickSelector('.encoding-link')
  const opened = await waitFor(`document.querySelector('.encoding-panel') !== null`, 3000)
  const pendingPaths = opened
    ? await ev(`[...document.querySelectorAll('.encoding-panel .encoding__path')].map((el) => el.textContent)`)
    : []
  check(
    '5.pending-list-has-gbk',
    pendingPaths.includes(GBK),
    { pendingPaths },
  )
  check(
    '5.pending-list-excludes-weird',
    !pendingPaths.includes(WEIRD),
    { pendingPaths },
  )
  await app.press('Escape', 'Escape', 27)
  await waitFor(`document.querySelector('.encoding-panel') === null`, 2000)
} else {
  check('5.pending-list-has-gbk', false, '待转存入口没出现：索引可能还没建完')
  check('5.pending-list-excludes-weird', false, '同上')
}

/* ═══════════════════════ 4. 删掉 >2MB 那篇 ═══════════════════════ */

fs.unlinkSync(abs(HUGE))
await reload()

const afterDeleteEntry = await sidebarText()
check('4.scan-side-unchanged', afterDeleteEntry?.trim() === `跳过 ${SCAN_EXPECTED}`, {
  entry: afterDeleteEntry,
})

const afterDelete = await waitForIndexPanel(
  (index) => index.rows.length === INDEX_EXPECTED - 1 && !index.rows.some((row) => row.path === HUGE),
)
const afterIndexPaths = (afterDelete?.index?.rows ?? []).map((row) => row.path)
check(
  '4.index-list-loses-one',
  afterIndexPaths.length === INDEX_EXPECTED - 1 && !afterIndexPaths.includes(HUGE),
  { afterIndexPaths, before: INDEX_EXPECTED },
)
check(
  '4.index-count-updated',
  afterDelete?.index?.heading.includes(`${INDEX_EXPECTED - 1} 篇笔记`) === true,
  { heading: afterDelete?.index?.heading ?? null },
)

/* ═══════════════════════ 7. 50 条上限（仅 Unix） ═══════════════════════ */

if (UNIX) {
  await closePanel()
  buildFixture({ badNames: 60 })
  await reload()

  const capEntry = await sidebarText()
  const capPanel = await openPanel()
  const capRows = capPanel?.scan?.rows ?? []
  check('7.cap-total-is-exact', capEntry?.trim() === `跳过 ${SCAN_EXPECTED + 60}`, {
    entry: capEntry,
    expected: `跳过 ${SCAN_EXPECTED + 60}`,
  })
  check('7.cap-list-is-50', capRows.length === 50, { rows: capRows.length })
  check(
    '7.cap-truncated-notice',
    (capPanel?.scan?.notices ?? []).some((text) => text.includes('只列出前 50 条')),
    { notices: capPanel?.scan?.notices ?? [] },
  )
  // 判据 6 的那条：lossy 路径必须带可见标记。
  const lossyRow = capRows.find((row) => row.flag !== null)
  check(
    '7.lossy-path-flagged',
    lossyRow !== undefined && lossyRow.flag.includes('无法显示'),
    { flag: lossyRow?.flag ?? null },
  )
  await closePanel()
} else {
  skip('7.cap-60-names', 'Windows 的 NTFS 文件名是 UTF-16，建不出非 UTF-8 的名字')
}

/* ═══════════════════════ 6. 0 条时入口不出现 ═══════════════════════ */

await closePanel()
unlockAll()
resetVault(VAULT, { 'only.md': '# 只剩这一篇\n' }, { clearTrash: true })
await reload()
await sleep(1500) // 让索引侧的数也归零

const emptyEntry = await sidebarText()
check('6.no-entry-at-zero', emptyEntry === null, { entry: emptyEntry })

// 顺带验「不许显示 0 个跳过」：整页里不该出现这个说法。
const zeroText = await ev(
  `[...document.querySelectorAll('.sidebar__foot *')].map((el) => el.textContent).join('|')`,
)
check('6.no-zero-label', !/跳过\s*0/.test(zeroText), { text: zeroText })

/* ═══════════════════════ 收尾 ═══════════════════════ */

check('X.运行期无 console error', consoleErrors.length === 0, consoleErrors.slice(0, 5))

unlockAll()
killApp()
await sleep(400)

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
console.log('')
console.log(
  `${failed.length === 0 ? '✅' : '❌'} 共 ${results.length} 项：${results.length - failed.length - skipped.length} 通过 / ${failed.length} 失败 / ${skipped.length} 跳过`,
)
if (skipped.length > 0) {
  console.log(`⚠ ${skipped.length} 项没有执行，本次验收不完整，不能当全绿看`)
}
fs.writeFileSync(
  path.join(OUT_DIR, `skip-list-acceptance-${Date.now()}.json`),
  JSON.stringify({ results, consoleErrors, platform: process.platform }, null, 2),
)
app.close()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
