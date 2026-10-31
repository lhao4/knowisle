// M7 第十八刀 · 库信息面板的端到端验收。
//
// # 这一条在验什么
//
// 「这个库现在是什么状态」此前没有出口：规模在侧栏那行小字里，索引状态只在
// 搜索面板顶部（不搜就看不见），回收站压了多少只有条数没有体积，索引库占了
// 多少磁盘**完全无处可查**。这一刀加一个**只读**面板，把四件事放一起。
//
// 判据（编号见验收报告 §5）：
//
//   0. 入口常驻、且**没有抢走** `.trash-link` 这个 e2e 定位钩子；
//   1. 面板打开后，路径 / 笔记数 / 目录数 / 上次扫描与侧栏那行统计逐项一致；
//   2. 索引库占用 > 0 **且等于磁盘上三个文件的和**、回收站 = 2.0 KB（1 个文件，
//      子目录不算）——两个数字都来自磁盘，口径都在判据里钉住；
//   3. 索引状态是四种文案之一，且带解释；
//   4. 三个出口：待转存 1 篇（点了真的打开编码面板）、跳过 0 时禁用、
//      忽略规则显示内置条数；
//   5. Esc 关闭，焦点回到编辑器；
//   6. **面板开着时，磁盘变了也不重扫**（避让生效）；关掉之后那一次改动
//      必须补扫（pending 回放）——后者是前者的反证；
//   7. 刷新幂等（数字不变、不报错）；全程无 console error。
//
// 前置（缺一条都不算数）：
//   1. `pnpm --filter @knowisle/desktop dev`（只有 vite，别用 `pnpm dev`）
//   2. `cargo build -p knowisle`（否则测的是旧二进制）
//   3. 应用**刚重启**（`--restart` 自己杀并起）
//
// 跑法：`node scripts/e2e/cdp-vault-info.mjs --restart`
// 退出码三种（B-19）：0 全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'

import { killApp, openApp } from './cdp.mjs'
import { INFO_VAULT as VAULT, OUT_DIR, sleep } from './env.mjs'
import { assertTempVault, resetVault } from './fixtures.mjs'

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
 * 回收站里那篇的**精确字节数**。
 *
 * 用 2048 而不是随便一个长度，是为了让断言能落在格式化之后的字符串上
 * （`2.0 KB`）：`formatBytes` 的进位规则（≥10 不留小数、<10 留一位）在这里
 * 被端到端钉了一次，改坏它会立刻红。
 */
const TRASH_BYTES = 2048

/**
 * 能解码的 GBK：进「待转存」，**不进**跳过清单。
 *
 * 字节与 `cdp-skip-list.mjs` 同一个：`# ` + GBK 的「你好世界」+ 换行 + GBK 的
 * 「中文正文」+ 换行。**必须用 Buffer**——Node 写字符串永远是 UTF-8。
 */
const GBK_HEX = '2320b4fdd7aab4e60ad6d0cec4d5fdcec40a'

function buildFixture() {
  resetVault(VAULT, {}, { clearTrash: true })

  for (const name of ['a', 'b', 'c']) {
    fs.mkdirSync(abs('notes'), { recursive: true })
    fs.writeFileSync(abs(`notes/${name}.md`), `# 笔记 ${name}\n\n正文。\n`, 'utf8')
  }
  fs.mkdirSync(abs('inbox'), { recursive: true })
  fs.writeFileSync(abs('inbox/hello.md'), '# 收件箱\n', 'utf8')

  fs.mkdirSync(abs('gbk'), { recursive: true })
  fs.writeFileSync(abs('gbk/legacy.md'), Buffer.from(GBK_HEX, 'hex'))

  // 回收站里放一篇：**只放文件，不放清单**。ADR-008 说「目录里有没有文件 =
  // 它还在不在」，清单只描述原路径；没有清单时它显示成「原路径未知」，但
  // 条数与体积照样算——这正是面板要数的两样东西。
  const trashDir = path.join(VAULT, '.knowisle', 'trash')
  fs.mkdirSync(trashDir, { recursive: true })
  const head = '# 回收站里的一篇\n'
  const filler = 'x'.repeat(TRASH_BYTES - Buffer.byteLength(head))
  fs.writeFileSync(path.join(trashDir, `${Date.now()}-gone.md`), head + filler, 'utf8')

  // 回收目录里塞一个**子目录**（里面还有一篇）：面板必须**不**数它。
  // 回收站是平的（`trash_list` 明写「目录与软链不是被删掉的笔记」），
  // 子目录只可能是用户自己放进去的；`trash_usage` 只数文件。
  // 没有这个干扰项，「数目录」这个变异在端到端上就咬不住（2b 会假绿）。
  fs.mkdirSync(path.join(trashDir, 'sub'), { recursive: true })
  fs.writeFileSync(path.join(trashDir, 'sub', 'nested.md'), 'y'.repeat(100), 'utf8')
}

/* ───────────────────────── 会话 ───────────────────────── */

const consoleErrors = []
assertTempVault(VAULT)

const session = await openApp({
  vault: VAULT,
  prepare: () => buildFixture(),
  errors: consoleErrors,
})
const { ev, waitFor } = session
await sleep(1800) // 让后台索引把 index.sqlite 落下来

/* ───────────────────────── 读面板的辅助 ───────────────────────── */

/**
 * 面板的全部可见状态，一次读回来。
 *
 * 用类名定位而不是第 n 个 `<section>`：排版顺序是给人看的，不该成为判据的
 * 一部分——调换两节的位置不该让用例红。
 */
const readPanel = () =>
  ev(`(() => {
    const panel = document.querySelector('.info-panel')
    if (panel === null) return null
    const cells = [...panel.querySelectorAll('.info__cell')].map((cell) => ({
      key: cell.querySelector('dt')?.textContent?.trim() ?? '',
      value: cell.querySelector('dd')?.textContent?.trim() ?? '',
    }))
    return {
      title: panel.querySelector('.trash__title')?.textContent?.trim() ?? '',
      path: panel.querySelector('.info__path')?.textContent?.trim() ?? '',
      cells,
      indexLabel: panel.querySelector('.info__label')?.textContent?.trim() ?? '',
      hints: [...panel.querySelectorAll('.info__hint')].map((h) => h.textContent.trim()),
      exits: [...panel.querySelectorAll('.info__exit')].map((b) => ({
        text: b.textContent.trim(),
        disabled: b.disabled,
      })),
      error: panel.querySelector('.trash__error')?.textContent?.trim() ?? null,
    }
  })()`)

/**
 * 脚本自己的一份 `formatBytes`，只为了和面板上的数字对账。
 *
 * **不复用应用的实现**：两边算错成同一个样子的概率，远小于一边改坏而另一
 * 边跟着一起坏。规则必须与 `src/lib/format.ts` 一致——≥10 不留小数、<10 留
 * 一位、字节不带小数、四舍五入之后满 1024 再进一位。
 */
function formatBytes(bytes) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  let value = Math.floor(bytes)
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  if (unit === 0) return `${value} B`
  const trim = (n) => n.toFixed(n >= 10 ? 0 : 1)
  const text = trim(value)
  if (Number(text) >= 1024 && unit < units.length - 1) {
    return `${trim(Number(text) / 1024)} ${units[unit + 1]}`
  }
  return `${text} ${units[unit]}`
}

/** `.knowisle/` 下索引库三个文件的字节和。缺一个就当 0（还没建出来）。 */
const diskIndexBytes = () =>
  ['index.sqlite', 'index.sqlite-wal', 'index.sqlite-shm'].reduce((sum, name) => {
    try {
      return sum + fs.statSync(path.join(VAULT, '.knowisle', name)).size
    } catch {
      return sum
    }
  }, 0)

/** 侧栏那行统计的原文与 title（title 是库根路径）。 */
const readStats = () =>
  ev(`(() => {
    const el = document.querySelector('.stats')
    if (el === null) return null
    return { text: el.textContent.trim(), root: el.title }
  })()`)

const openPanel = async () => {
  await ev(`document.querySelector('.info-link').click(); true`)
  return waitFor(`document.querySelector('.info-panel') !== null`, 5000)
}

const closeByEsc = async () => {
  await ev(`(() => {
    const panel = document.querySelector('.info-panel')
    panel?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    return true
  })()`)
  return waitFor(`document.querySelector('.info-panel') === null`, 3000)
}

/**
 * 装一个「扫了几次」的计数器。
 *
 * 与 `cdp-tree-probe.mjs` 同一套：每次重扫都会把 `.rescan-btn` 置成
 * `disabled`，数这个跃迁就等于数扫描次数。它是**生产行为**，不是插桩。
 */
const installScanCounter = () =>
  ev(`(() => {
    const btn = document.querySelector('.rescan-btn')
    if (btn === null) return false
    window.__kiScans = 0
    new MutationObserver(() => {
      if (btn.disabled) window.__kiScans += 1
    }).observe(btn, { attributes: true, attributeFilter: ['disabled'] })
    return true
  })()`)

/**
 * 打开 `notes/a.md`，等 breadcrumb 出现它的名字。
 *
 * 5c 判的是「面板关掉之后焦点回到编辑器」，而**编辑器只有打开一篇笔记才存在**。
 * 不先开一篇，`.cm-content` 根本不在 DOM 里，`focusEditor()` 无处可落——
 * 那时 5c 红的是脚本自己的前提，不是实现。这条前提单独占一个判据（0e），
 * 免得它失败时表现成「焦点回归坏了」。
 */
const openNote = async (rel) => {
  const dir = rel.split('/')[0]
  await ev(`(() => {
    const el = [...document.querySelectorAll('button.tree-row--dir')]
      .find((b) => b.title === ${JSON.stringify(dir)})
    if (el !== null && el.getAttribute('aria-expanded') !== 'true') el.click()
    return true
  })()`)
  const visible = await waitFor(
    `[...document.querySelectorAll('button.tree-row--file')].some((b) => b.title === ${JSON.stringify(rel)})`,
    4000,
  )
  if (!visible) return false
  await ev(`(() => {
    [...document.querySelectorAll('button.tree-row--file')]
      .find((b) => b.title === ${JSON.stringify(rel)})
      ?.click()
    return true
  })()`)
  return waitFor(`document.querySelector('.cm-content') !== null`, 5000)
}

/* ═══════════════════════ 0. 入口 ═══════════════════════ */

const stats = await readStats()
check('0a 夹具与侧栏统计就位', stats !== null && /^5 篇 · 3 个目录 · \d+ ms$/.test(stats.text), {
  stats: stats?.text ?? null,
})

const entry = await ev(`(() => {
  const el = document.querySelector('.info-link')
  return el === null ? null : { text: el.textContent.trim(), title: el.title }
})()`)
check('0b 入口常驻（不依赖任何非零计数）', entry?.text === '库信息', entry)

/*
 * 这条守的是 e2e 自己的钩子：`.trash-link` 是 `cdp-trash*.mjs` 的定位选择器
 * （`querySelector` 取第一个匹配）。新入口一旦顺手带上这个类，三个回收站
 * 脚本就会点开库信息面板，然后红在「列表里 0 行」这种完全对不上的地方。
 */
const trashLinkText = await ev(
  `document.querySelector('.trash-link')?.textContent?.trim() ?? null`,
)
check('0c 新入口没有抢走 .trash-link 钩子', trashLinkText?.startsWith('回收站') === true, {
  trashLink: trashLinkText,
})

check('0d 扫描计数器装上了', await installScanCounter(), {})

/*
 * 先开一篇笔记：5c 要验「焦点回到编辑器」，而没有打开任何笔记时编辑器
 * 压根不渲染。这条是 5c 的前提，失败必须自己报出来（见 openNote 的注释）。
 * 挑 `notes/a.md` 而不是 `gbk/legacy.md`——后者会顺手弹出「待转存」横幅。
 */
check('0e 打开 notes/a.md（5c 的前提：编辑器存在）', await openNote('notes/a.md'), {})

/* ═══════════════════════ 1. 面板内容与侧栏一致 ═══════════════════════ */

check('1a 点开之后面板出现', await openPanel(), {})

const panel = await readPanel()
check('1b 标题是「库信息」', panel?.title === '库信息', { title: panel?.title ?? null })

check('1c 路径与侧栏 title 逐字相同', panel?.path === stats.root, {
  panel: panel?.path ?? null,
  sidebar: stats.root,
})

const cell = (key) => panel?.cells.find((c) => c.key === key)?.value ?? null
check('1d 笔记数与侧栏一致', cell('笔记') === '5 篇', { cell: cell('笔记') })
check('1e 目录数与侧栏一致', cell('目录') === '3 个', { cell: cell('目录') })
check('1f 上次扫描是毫秒数（不是「未知」）', /^\d+ ms$/.test(cell('上次扫描') ?? ''), {
  cell: cell('上次扫描'),
})

/* ═══════════════════════ 2. 两个占用数字来自磁盘 ═══════════════════════ */

/*
 * 索引库可能还没落盘（后台建索引）。这不是「面板错了」，所以等它，而不是
 * 直接判失败——但**等到位才判**，绝不「睡够了就断言」。
 */
let indexCell = cell('索引库')
for (let attempt = 0; attempt < 6 && indexCell === '0 B'; attempt += 1) {
  await ev(`[...document.querySelectorAll('.info-panel button')].find((b) => b.textContent.trim() === '刷新')?.click(); true`)
  await sleep(700)
  indexCell = (await readPanel())?.cells.find((c) => c.key === '索引库')?.value ?? null
}
check('2a 索引库占用 > 0 且格式是「数字 + 单位」', /^\d+(\.\d)? (B|KB|MB|GB)$/.test(indexCell ?? '') && indexCell !== '0 B', {
  indexBytes: indexCell,
})

/*
 * 2a′ 把口径钉在磁盘上：面板上那个数必须等于 `index.sqlite` + `-wal` + `-shm`
 * **三个文件**的和，格式化之后逐字相等。
 *
 * 只判「> 0」抓不到最可能的那个回归——「只算主库」。WAL 模式下正文几乎全在
 * `-wal` 里，只算主库照样 > 0，于是断言绿着，数字却少了一个数量级。这里用
 * 脚本自己实现的一份 `formatBytes` 去对账，不复用应用的实现：两边算错成
 * 同一个样子的概率，远小于一边改坏。
 *
 * 索引建完之后 `-wal` 不再长，所以等式是稳定的；万一撞上后台还在写，
 * 重试几次再判，而不是放宽成区间——区间会让这条判据失去意义。
 */
let diskMatch = false
for (let attempt = 0; attempt < 4 && !diskMatch; attempt += 1) {
  const disk = formatBytes(diskIndexBytes())
  const shown = (await readPanel())?.cells.find((c) => c.key === '索引库')?.value ?? null
  diskMatch = shown === disk
  if (!diskMatch) {
    await ev(`[...document.querySelectorAll('.info-panel button')].find((b) => b.textContent.trim() === '刷新')?.click(); true`)
    await sleep(500)
  }
}
check('2a′ 索引库 = 磁盘上三个文件的字节和（不是只算主库）', diskMatch, {
  disk: formatBytes(diskIndexBytes()),
  shown: indexCell,
})

const afterIndex = await readPanel()
const trashCell = afterIndex?.cells.find((c) => c.key === '回收站')?.value ?? null
check('2b 回收站 = 2.0 KB（1 个文件）', trashCell === '2.0 KB（1 个文件）', { trash: trashCell })

check('2c 索引状态是四种文案之一，且带解释', (() => {
  const label = afterIndex?.indexLabel ?? ''
  const known =
    label === '还没开始' || label === '已就绪' || label === '建立失败' || label.startsWith('正在建立')
  const explained = (afterIndex?.hints ?? []).some((h) => h.length > 10)
  return known && explained
})(), { label: afterIndex?.indexLabel ?? null })

/* ═══════════════════════ 3. 三个出口 ═══════════════════════ */

const exits = afterIndex?.exits ?? []
check('3a 待转存出口显示 1 篇', exits[1]?.text === '待转存 1 篇', { exits })
check('3b 跳过为 0 时出口禁用且说清', exits[0]?.text === '没有条目被跳过' && exits[0]?.disabled === true, {
  exit: exits[0],
})
check('3c 忽略规则显示内置条数', /^忽略规则：内置 \d+ 条/.test(exits[2]?.text ?? ''), {
  exit: exits[2]?.text ?? null,
})

/* ═══════════════════════ 4. 出口真的能跳过去 ═══════════════════════ */

await ev(`document.querySelectorAll('.info__exit')[1].click(); true`)
const encodingOpened = await waitFor(`document.querySelector('.encoding-panel') !== null`, 5000)
check('4a 点「待转存」打开编码面板', encodingOpened, {})
check('4b 库信息面板同时关掉了（不叠两层模态）', (await readPanel()) === null, {})

/*
 * 关掉编码面板：它**没有「关闭」按钮**（头部只有「全部转存」/「取消」），
 * 唯一出口是 Esc。
 *
 * 这里必须**断言关掉了**，而不是点一下就走：一个没关掉的模态会一直挂在
 * `probeBlocked` 上（`encodingOpen === true`），于是第 6 节的探针永远被避让
 * 挡掉——「面板开着不重扫」会假绿，「关掉之后补扫」会假红，而红的地方看起来
 * 像是避让坏了。这条断言就是那次的教训。
 */
await ev(`(() => {
  const panel = document.querySelector('.encoding-panel')
  panel?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
  return true
})()`)
check('4c 编码面板已关闭（否则第 6 节的避让判据全都会失真）', await waitFor(`document.querySelector('.encoding-panel') === null`, 3000), {})

/* ═══════════════════════ 5. Esc 关闭 + 焦点归还 ═══════════════════════ */

check('5a 重新打开', await openPanel(), {})
check('5b Esc 关闭', await closeByEsc(), {})

let focused = false
for (let attempt = 0; attempt < 12 && !focused; attempt += 1) {
  focused = await ev(`document.activeElement?.classList.contains('cm-content') === true`)
  if (!focused) await sleep(80)
}
check('5c 关闭后焦点回到编辑器', focused, {})

/* ═══════════════════════ 6. 面板开着时不重扫（避让） ═══════════════════════ */

/*
 * 这一节必须**真的改磁盘**，而且必须先证「探针本身在工作」。
 *
 * 只切窗口、不动磁盘的话，指纹本来就没变——探针跑与不跑都不会重扫，于是
 * 「scans 没涨」对「避让是否生效」一无所知。变异验证时把 `infoOpen` 从
 * `probeBlocked` 里删掉，只切窗口的版本照样 PASS（假绿），就是这么来的。
 *
 * 所以三步：① 面板关着、磁盘变了 → **必须扫**（探针在工作）；② 面板开着、
 * 磁盘又变了 → **不许扫**（避让）；③ 关掉面板 → 那次被挡的改动**补扫**
 * （pending 回放）。②没有①就只是「什么都没发生」，③是②的反证。
 */
const awayBack = async () => {
  await ev(`window.dispatchEvent(new Event('blur')); true`)
  await sleep(1200)
  await ev(`window.dispatchEvent(new Event('focus')); true`)
}

// ① 对照：面板关着
const scansBase = await ev(`window.__kiScans ?? -1`)
fs.writeFileSync(abs('notes/d.md'), '# 外面新建的\n', 'utf8')
await awayBack()
check('6a 对照：面板关着时，磁盘变了会重扫（探针在工作）', await waitFor(`(window.__kiScans ?? 0) > ${scansBase}`, 15000), {
  scans: [scansBase, await ev(`window.__kiScans ?? -1`)],
})

// ② 避让：面板开着，磁盘再变一次
await openPanel()
const scansBefore = await ev(`window.__kiScans ?? -1`)
fs.writeFileSync(abs('notes/e.md'), '# 面板开着时新建的\n', 'utf8')
await awayBack()
await sleep(600)
const scansAfter = await ev(`window.__kiScans ?? -1`)
check('6b 面板开着时，磁盘变了也不重扫', scansAfter === scansBefore, {
  scans: [scansBefore, scansAfter],
})
check('6c 面板还开着（避让没有副作用）', (await readPanel()) !== null, {})

// ③ 回放：关掉面板，那次被挡掉的改动必须补上
await closeByEsc()
check('6d 面板关掉后补扫一次（pending 回放，6b 的反证）', await waitFor(`(window.__kiScans ?? 0) > ${scansAfter}`, 15000), {
  scans: await ev(`window.__kiScans ?? -1`),
})

// 重扫之后树里是 7 篇。第 7 节从这里重新读面板。
await openPanel()

/* ═══════════════════════ 7. 刷新幂等 ═══════════════════════ */

const beforeRefresh = await readPanel()
await ev(`[...document.querySelectorAll('.info-panel button')].find((b) => b.textContent.trim() === '刷新')?.click(); true`)
await sleep(700)
const afterRefresh = await readPanel()

check('7a 刷新之后数字不变', JSON.stringify(beforeRefresh?.cells) === JSON.stringify(afterRefresh?.cells), {
  before: beforeRefresh?.cells ?? null,
  after: afterRefresh?.cells ?? null,
})
check('7b 刷新没有产生错误行', afterRefresh?.error === null, { error: afterRefresh?.error ?? null })

await closeByEsc()
await sleep(300)

check('X.运行期无 console error', consoleErrors.length === 0, consoleErrors.slice(0, 5))

/* ═══════════════════════ 收尾 ═══════════════════════ */

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
  path.join(OUT_DIR, `vault-info-acceptance-${Date.now()}.json`),
  JSON.stringify({ results, consoleErrors, platform: process.platform }, null, 2),
)
session.close()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
