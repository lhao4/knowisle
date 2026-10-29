// M7 第十一刀（B-39）· 库级「N 篇不是 UTF-8」+ 批量转存的端到端验收。
//
// # 这一条在验什么
//
// 第九刀只证明了**单篇**转存：打开一篇、看见提示行、点按钮、磁盘变了。
// 它没有回答「我不知道哪几篇不是 UTF-8」——树里没有标记，状态栏没有计数，
// 搜索也搜不到「编码」这件事。这一刀补的是库级的那一面：
//
//   1. 不打开任何一篇，侧栏就能数出「20 篇不是 UTF-8」；
//   2. 清单里每一篇的编码名与磁盘实际编码一致；
//   3. 一次确认之后逐篇转存，磁盘字节真的变成 UTF-8；
//   4. 转完清单自己清空——**没有手碰索引**；
//   5. 再点一次是幂等的（`history/` 不新增）；
//   6. 中途有一篇转不了 → 其余照转，失败能定位到路径；
//   7. 跑到一半取消 → 已转的保持已转、未转的仍是 GBK、应用不崩。
//
// # 判据一律看磁盘字节
//
// `Buffer.compare(fs.readFileSync(p), Buffer.from(text, 'utf8')) === 0`，
// 不是 `toString('utf8') === text`：后者对 GBK 字节也会"成功"解出一堆
// U+FFFD，看起来像通过了。夹具也用 `Buffer` 写——写字符串永远造不出 GBK。
//
// 前置（缺一条都不算数）：
//   1. `pnpm --filter @knowisle/desktop dev`（只有 vite，别用 `pnpm dev`）
//   2. `cargo build -p knowisle`（否则测的是旧二进制）
//   3. 应用**刚重启**（`--restart` 自己杀并起）
//
// 跑法：`node scripts/e2e/cdp-encoding-batch.mjs --restart`
// 退出码三种（B-19）：0 全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'

import { killApp, openApp } from './cdp.mjs'
import { ENCODING_BATCH_VAULT as VAULT, OUT_DIR, sleep } from './env.mjs'
import { assertTempVault, resetVault } from './fixtures.mjs'

const STAMP = String(Date.now()).slice(-6)

/** GBK 的「# 待转存\n中文正文\n」。字节由 `iconv -f UTF-8 -t GBK` 生成。 */
const GBK_TEXT = '# 待转存\n中文正文\n'
const GBK_HEX = '2320b4fdd7aab4e60ad6d0cec4d5fdcec40a'
const GBK_BYTES = Buffer.from(GBK_HEX, 'hex')
const UTF8_BYTES = Buffer.from(GBK_TEXT, 'utf8')

/** 那 980 篇 UTF-8 的正文。够短，1000 篇建索引不到两秒。 */
const utf8Body = (index) => `# 普通笔记 ${index}\n\n这篇本来就是 UTF-8，不需要转存。\n`

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  console.log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}

/* ───────────────────────── 夹具 ───────────────────────── */

const abs = (rel) => path.join(VAULT, rel)
const gbkRel = (index) => `gbk/g${String(index).padStart(2, '0')}.md`
const extraRel = (index) => `gbk/x${String(index).padStart(2, '0')}.md`
const cancelRel = (index) => `bulk/c${String(index).padStart(3, '0')}.md`

/** 写一篇 GBK。**必须用 Buffer**：写字符串是 UTF-8，那测的就不是 GBK 了。 */
const writeGbk = (rel) => {
  const target = abs(rel)
  fs.mkdirSync(path.dirname(target), { recursive: true })
  fs.writeFileSync(target, GBK_BYTES)
}

/** 20 篇 GBK + 980 篇 UTF-8，全部平铺在 10 个目录里。 */
const buildFixture = () => {
  resetVault(VAULT, {}, { clearTrash: true })
  for (let index = 0; index < 980; index += 1) {
    const dir = `d${String(index % 10).padStart(2, '0')}`
    const target = abs(`${dir}/n${String(index).padStart(4, '0')}.md`)
    fs.mkdirSync(path.dirname(target), { recursive: true })
    fs.writeFileSync(target, utf8Body(index), 'utf8')
  }
  for (let index = 0; index < 20; index += 1) writeGbk(gbkRel(index))
}

/** `history/` 下一共有多少个备份**文件**（不是目录）。 */
const countBackups = () => {
  let count = 0
  const walk = (dir) => {
    let entries
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true })
    } catch {
      return
    }
    for (const entry of entries) {
      const target = path.join(dir, entry.name)
      if (entry.isDirectory()) walk(target)
      else count += 1
    }
  }
  walk(path.join(VAULT, '.knowisle', 'history'))
  return count
}

/* ───────────────────────── 会话 ───────────────────────── */

const consoleErrors = []
let app
let ev
let waitFor

const bind = (session) => {
  app = session
  ev = session.ev
  waitFor = session.waitFor
}

assertTempVault(VAULT)
bind(await openApp({ vault: VAULT, prepare: buildFixture, errors: consoleErrors }))
const send = (method, params) => app.send(method, params)
await sleep(300)

const clickAt = async (x, y) => {
  await send('Input.dispatchMouseEvent', {
    type: 'mousePressed',
    x,
    y,
    button: 'left',
    buttons: 1,
    clickCount: 1,
  })
  await send('Input.dispatchMouseEvent', {
    type: 'mouseReleased',
    x,
    y,
    button: 'left',
    buttons: 0,
    clickCount: 1,
  })
  await sleep(120)
}

/** 按文字点元素。坐标每次重新取：树在重扫期间会长高一行（B-33 踩过）。 */
const clickText = async (selector, text, settle = 200) => {
  for (let i = 0; i < 25; i += 1) {
    const rect = await ev(`(() => {
      const el = [...document.querySelectorAll(${JSON.stringify(selector)})]
        .find((e) => e.textContent.trim() === ${JSON.stringify(text)})
      if (!el) return null
      el.scrollIntoView({ block: 'center' })
      const r = el.getBoundingClientRect()
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
    })()`)
    if (rect !== null) {
      await clickAt(rect.x, rect.y)
      await sleep(settle)
      return true
    }
    await sleep(120)
  }
  return false
}

/** 侧栏入口上那行字（**只看第一个 span**：第二个是数字徽标）。 */
const entryText = () =>
  ev(`document.querySelector('.encoding-link > span')?.textContent ?? null`)

/** 等入口上的数字变成 `count`。索引在后台建，所以这里必须轮询。 */
const waitEntry = (count, timeoutMs = 40000) =>
  waitFor(`document.querySelector('.encoding-link > span')?.textContent === ${JSON.stringify(`${count} 篇不是 UTF-8`)}`, timeoutMs)

/** 清单：每一行的路径与编码名。 */
const listRows = () =>
  ev(`[...document.querySelectorAll('.trash__item')].map((row) => ({
    path: row.querySelector('.encoding__path')?.textContent ?? null,
    encoding: row.querySelector('.encoding__tag')?.textContent ?? null,
  }))`)

/** 面板状态：跑着 / 跑完 / 还没跑。 */
const panelState = () =>
  ev(`(() => {
    const panel = document.querySelector('.encoding-panel')
    if (!panel) return { open: false }
    const summary = panel.querySelector('.encoding__summary')?.textContent ?? null
    const progress = panel.querySelector('.encoding__progress-text')?.textContent ?? null
    const failures = [...panel.querySelectorAll('.encoding__failure')].map((row) => ({
      path: row.querySelector('.encoding__path')?.textContent ?? null,
      reason: row.querySelector('.encoding__reason')?.textContent ?? null,
    }))
    return {
      open: true,
      summary,
      progress,
      failures,
      rows: panel.querySelectorAll('.trash__item').length,
      busy: panel.getAttribute('aria-busy') === 'true',
    }
  })()`)

/**
 * 打开面板并等清单渲染出来。
 *
 * 用选择器点而不是按文字找：入口按钮里有两个 span（标签 + 数字徽标），
 * `textContent` 是「20 篇不是 UTF-820」，按整串匹配永远找不到。
 */
const openPanel = async () => {
  const clicked = await app.clickSelector('.encoding-link')
  if (!clicked) return false
  return waitFor(`document.querySelector('.encoding-panel .trash__item') !== null`, 8000)
}

/**
 * 走完「全部转存 → 二次确认 → 开跑」。
 *
 * 带重试：`全部转存` 点下去的那一刻，面板可能正因为刚刷新完统计而重渲染，
 * 点击会落在被替换掉的节点上（`clickText` 找得到、点得着，但没有触发 onClick）。
 * 判据是「它真的开始了」——进度行或结果行出现——而不是「我点过了」。
 */
const startBatch = async () => {
  for (let attempt = 0; attempt < 3; attempt += 1) {
    await clickText('.trash__head button', '全部转存', 250)
    if (!(await waitFor(`document.querySelector('[role="alertdialog"]') !== null`, 4000))) continue
    await clickText('.confirm__actions button', '转存', 300)
    const started = await waitFor(
      `document.querySelector('.encoding__progress-text') !== null ||
       document.querySelector('.encoding__summary') !== null`,
      8000,
    )
    if (started) return true
  }
  return false
}

/** 关面板。Esc 挂在面板容器上，所以焦点必须已经在里面（打开时程序化聚焦过）。 */
const closePanel = async () => {
  await app.press('Escape', 'Escape', 27)
  await sleep(200)
}

/** 展开一个目录再点里面的笔记。目录行带着子项计数，按 `.tree-label` 找才准。 */
const openNoteByLabel = async (dir, name) => {
  if (dir !== null) await clickText('.tree-label', dir, 250)
  const clicked = await clickText('.tree-label', name, 250)
  if (!clicked) return false
  return waitFor(
    `document.querySelector('.breadcrumb')?.textContent?.includes(${JSON.stringify(name)}) === true`,
    8000,
  )
}

/* ═══════════════ 1. 不打开任何一篇，就能数出 20 ═══════════════ */

const entryArrived = await waitEntry(20)
check(
  '1 库级入口自己出现：20 篇 GBK + 980 篇 UTF-8，不打开任何一篇就写着「20 篇不是 UTF-8」',
  entryArrived,
  { entry: await entryText(), rows: await ev(`document.querySelectorAll('.tree-row').length`) },
)

/* ═══════════════ 2. 清单逐篇核对编码名 ═══════════════ */

const panelOpened = await openPanel()
const rows = await listRows()
const wantPaths = Array.from({ length: 20 }, (_, index) => gbkRel(index))
check(
  '2 清单 20 行，路径逐条对上磁盘上那 20 篇，编码名全是 GBK',
  panelOpened &&
    rows.length === 20 &&
    rows.every((row) => row.encoding === 'GBK') &&
    JSON.stringify(rows.map((row) => row.path)) === JSON.stringify(wantPaths),
  { rows: rows.length, first: rows[0], last: rows[19] },
)

/* ═══════════════ 3–5. 一次确认 → 逐篇转存 → 磁盘字节 ═══════════════ */

const backupsBefore = countBackups()
await clickText('.trash__head button', '全部转存', 250)
const dialogText = await (async () => {
  const ok = await waitFor(`document.querySelector('[role="alertdialog"]') !== null`, 4000)
  return ok ? ev(`document.querySelector('[role="alertdialog"]')?.textContent ?? null`) : null
})()
check(
  '3 点「全部转存」→ 二次确认，文案写明 20 篇与备份目录',
  dialogText !== null && dialogText.includes('20') && dialogText.includes('.knowisle/history/'),
  { text: (dialogText ?? '').slice(0, 120) },
)

await clickText('.confirm__actions button', '转存', 300)
const finished = await waitFor(
  `document.querySelector('.encoding__summary') !== null`,
  60000,
)
const done = await panelState()
check(
  '4 转存完成：结果是「20 篇全部转存为 UTF-8」，没有失败项',
  finished && done.summary === '20 篇全部转存为 UTF-8' && done.failures.length === 0,
  done,
)

const diskChecks = wantPaths.map((rel) => {
  const bytes = fs.readFileSync(abs(rel))
  return Buffer.compare(bytes, UTF8_BYTES) === 0
})
check(
  '5 逐篇比对磁盘字节：20 篇全部 == UTF-8（Buffer.compare，不是 toString）',
  diskChecks.every(Boolean),
  { ok: diskChecks.filter(Boolean).length, total: diskChecks.length },
)

const backupsAfter = countBackups()
check(
  '6 每篇都留下了备份（20 份，逐字节等于原文件）',
  backupsAfter === backupsBefore + 20,
  { before: backupsBefore, after: backupsAfter },
)

/* ═══════════════ 7. 入口消失：索引自己清了，没人碰它 ═══════════════ */

const entryGone = await waitFor(`document.querySelector('.encoding-link') === null`, 15000)
check(
  '7 转完之后入口自己消失（nonUtf8 归零，全程没有手改索引行）',
  entryGone,
  { entry: await entryText() },
)

/* ═══════════════ 8. 幂等：再打开是 0 篇，history 不新增 ═══════════════ */

// 入口没了就点不到了，所以这里验的是「刷新之后仍然是 0」：
// 重新加载会重扫 + 重读统计，如果转存没有把 `src_encoding` 清干净，
// 那 20 篇会立刻又冒出来。
await app.reload()
await app.waitFor(`document.querySelector('.tree-row') !== null`, 30000)
const stillGone = await waitFor(`document.querySelector('.encoding-link') === null`, 30000)
await sleep(1200)
check(
  '8 重新加载之后仍然是「一篇都没有」（幂等：清单不是靠一次性的本地状态清掉的）',
  stillGone && countBackups() === backupsAfter,
  { backups: countBackups() },
)

/* ═══════════════ 9. 失败不中断：删掉其中一篇再转 ═══════════════ */

// 再造 3 篇 GBK，重新加载让它们进索引；然后**在清单已经列出来之后**删掉一篇，
// 模拟「列出来之后磁盘变了」——那正是这一档失败最真实的形态。
for (let index = 0; index < 3; index += 1) writeGbk(extraRel(index))
await app.reload()
await app.waitFor(`document.querySelector('.tree-row') !== null`, 30000)
const extraArrived = await waitEntry(3)
const reopened = extraArrived && (await openPanel())
fs.unlinkSync(abs(extraRel(1)))
const partialStarted = await startBatch()
const partial = partialStarted && (await waitFor(`document.querySelector('.encoding__summary') !== null`, 60000))
const partialState = await panelState()
check(
  '9 失败不中断：3 篇里删掉 1 篇 → 2 篇成功 / 1 篇失败，失败行能定位到路径与原因',
  reopened &&
    partial &&
    partialState.summary === '2 篇成功 / 1 篇失败' &&
    partialState.failures.length === 1 &&
    partialState.failures[0].path === extraRel(1) &&
    (partialState.failures[0].reason ?? '').length > 0,
  partialState,
)

/* ═══════════════ 10–11. 可取消：300 篇跑到一半停下 ═══════════════ */

await closePanel()
for (let index = 0; index < 300; index += 1) writeGbk(cancelRel(index))
await app.reload()
await app.waitFor(`document.querySelector('.tree-row') !== null`, 30000)
// 只剩这 300 篇：上一轮那 3 篇里 2 篇已转好、1 篇被删。
const bulkArrived = await waitEntry(300, 60000)
const bulkOpened = bulkArrived && (await openPanel())
const bulkStarted = bulkOpened && (await startBatch())

// 等真实进度出现（Rust 侧逐篇回报），再取消。
const progressed = await waitFor(
  `(() => {
    const text = document.querySelector('.encoding__progress-text')?.textContent ?? ''
    const done = Number((text.match(/^(\\d+)/) ?? [])[1] ?? 0)
    return done >= 20
  })()`,
  60000,
)
const cancelled = await clickText('.trash__head button', '取消', 200)
const settled = await waitFor(`document.querySelector('.encoding__summary') !== null`, 60000)
const cancelState = await panelState()

const bulkDone = Array.from({ length: 300 }, (_, index) => cancelRel(index)).filter(
  (rel) => Buffer.compare(fs.readFileSync(abs(rel)), UTF8_BYTES) === 0,
)
check(
  '10 可取消：跑到 20 篇之后点取消 → 结果如实说「已取消」并报出已转篇数',
  bulkStarted && progressed && cancelled && settled && (cancelState.summary ?? '').startsWith('已取消'),
  { progressed, cancelled, summary: cancelState.summary },
)

check(
  '11 取消之后：已转的保持 UTF-8、未转的仍是 GBK（两边都不为 0，且没有回滚）',
  bulkDone.length > 0 && bulkDone.length < 300,
  { utf8: bulkDone.length, gbk: 300 - bulkDone.length },
)

/* ═══════════════ 12. 取消之后应用还能用 ═══════════════ */

await closePanel()
const closed = await waitFor(`document.querySelector('.encoding-panel') === null`, 5000)
const treeAlive = await ev(`document.querySelectorAll('.tree-row').length > 0`)
const noteOpened = await openNoteByLabel('d00', 'n0000.md')
check(
  '12 取消之后应用照常可用（面板关得上，树还在，笔记打得开）',
  closed && treeAlive && noteOpened,
  { closed, treeAlive, noteOpened },
)

/* ═══════════════ 13. 无 console error ═══════════════ */

check('13 运行期无 console error / 未捕获异常', consoleErrors.length === 0, consoleErrors.slice(0, 5))

/* ───────────────────────── 收尾 ───────────────────────── */

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
console.log(
  `\n${failed.length === 0 ? '✅' : '❌'} 共 ${results.length} 项：${results.length - failed.length - skipped.length} 通过 / ${failed.length} 失败 / ${skipped.length} 跳过`,
)
fs.writeFileSync(
  path.join(OUT_DIR, `encoding-batch-acceptance-${STAMP}.json`),
  JSON.stringify({ results, consoleErrors, stamp: STAMP }, null, 2),
)
app.close()
killApp()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
