// M5 第三刀 · 回收站分页的端到端验收（真实 WebView2 + CDP）。
//
// 覆盖交付验收 1、2、4、7：
//   1. 10k 条夹具上首屏 200 行、total = 10000、`next` 非空（底部出现「加载更多」）
//   2. 点「加载更多」到 400 行；恢复一条之后继续翻，仍然不重不漏
//   4. 恢复**中间那一页**的一条：面板不关、该行消失、total 减 1、再翻页不重复
//   7. 清空：删掉全部（不是当前页），total = 0
//
// 第 4 条是 keyset 与 offset 的分水岭，所以这里不只数行数，还比对**集合**：
// offset 分页在恢复一条之后会跳过一条，而行数看起来一模一样（599 = 599）。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`：vite 在 127.0.0.1:1420（脚本会取 /src/App.tsx 核对转译结果，B-18）
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16：调试槽位是进程级静态）
//      或 `pnpm e2e:trash-page --restart` 让脚本自己杀掉并重启
//   4. 运行期间源码冻结
//
// 夹具由 Rust 侧造（`%TEMP%\ki-e2e-trash-page`，10k 条 + 1 篇笔记）：
// 本机 node 的 `rmSync` 单次超过 50 个目标就要人工确认（B-19 / TOOLING），
// 两万个文件清不掉，所以夹具的构造与清理都放在 `tests/trash_page.rs` 里。
// **夹具必须放系统临时目录**：本机实测仓库 `.tmp` 里删一个文件 390ms，
// `%TEMP%` 里 1.8ms——两万个文件的清空在 `.tmp` 里要两小时。
//
// 跑法：`pnpm e2e:trash-page`（或加 `--restart`）
// 退出码：0 全过 / 1 有失败。
import fs from 'node:fs'
import path from 'node:path'
import { execSync } from 'node:child_process'

import { openApp, killApp } from './cdp.mjs'
import { OUT_DIR, REPO, TRASH_PAGE_VAULT as VAULT } from './env.mjs'

const TRASH = path.join(VAULT, '.knowisle', 'trash')
const TOTAL = 10_000
/** 恢复哪一条：第 250 个（0 起）——正好在第二页中间。 */
const RESTORE_AT = 250
const STAMP = String(Date.now()).slice(-6)

const log = (...a) => console.log(...a)

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}

/* ───────────────────────── 夹具 ───────────────────────── */

/** 夹具里第 `index` 条的清单 origin（0 = 最旧）。 */
const originOf = (index) => `notes/n${String(index).padStart(5, '0')}.md`

/** 倒序前 `count` 条的 origin（最新的在前）。 */
const newest = (count) => Array.from({ length: count }, (_, k) => originOf(TOTAL - 1 - k))

/**
 * 10k 条夹具（构造与清理都在 Rust 侧，见文件头）。
 *
 * 它会 `remove_dir_all` 整个夹具库。应用开着这个库时 `index.sqlite` 被占用，
 * Windows 上会失败——那种情况下把病因说清楚，而不是抛一个 EBUSY 让人猜。
 */
const buildFixture = () => {
  log('造夹具（10k 条，Rust 侧）…')
  try {
    execSync('cargo test -p ki-vault --test trash_page prepare_e2e_fixture -- --ignored --nocapture', {
      cwd: REPO,
      stdio: 'inherit',
      env: { ...process.env, KI_E2E_FIXTURE: VAULT },
    })
  } catch (err) {
    throw new Error(
      `夹具构造失败。若报 EBUSY / Access denied，说明应用还占着 ${VAULT} 里的 index.sqlite：` +
        `先关掉应用，或加 --restart 让脚本自己重启。\n${err.message}`,
    )
  }
  const count = fs.readdirSync(TRASH).length
  if (count !== TOTAL) throw new Error(`夹具不对：回收目录里 ${count} 条，期望 ${TOTAL}`)
  log(`夹具就绪：${VAULT}（${count} 条）`)
}

/* ───────────────────────────── 会话 ───────────────────────────── */

const consoleErrors = []
let app
let ev
let waitFor
let clickAt
let clickSelector

const bind = (session) => {
  app = session
  ev = session.ev
  waitFor = session.waitFor
  clickAt = session.clickAt
  clickSelector = session.clickSelector
}

const rows = () => ev(`document.querySelectorAll('.trash__item').length`)
const countText = () => ev(`document.querySelector('.trash__count')?.textContent ?? null`)
const progressText = () => ev(`document.querySelector('.trash__progress')?.textContent ?? null`)
const origins = () => ev(`[...document.querySelectorAll('.trash__origin')].map((e) => e.textContent)`)

/* ───────────────────────── 验收 ───────────────────────── */

const run = async () => {
  bind(await openApp({ vault: VAULT, prepare: buildFixture, errors: consoleErrors }))
  check('0 树已渲染（夹具库打开成功）', true, {})

  // ── 1. 首屏 200 行 ──
  const t0 = Date.now()
  const clicked = await clickSelector('.trash-link')
  check('1a 点开了回收站入口', clicked, {})
  const firstPage = await waitFor(`document.querySelectorAll('.trash__item').length === 200`, 60000)
  const elapsed = Date.now() - t0
  check('1b 首屏正好 200 行', firstPage, { rows: await rows(), ms: elapsed })
  // 6 秒，不是 3 秒：这 2.6 秒里装的是**首屏扫描 10k 篇**，不是分页本身
  // （M5 记的首屏 120ms 是列表渲染，不含冷扫描）。3 秒在慢一档的机器上
  // 就是一条随机红，而 TOOLING 的规矩是性能断言只卡灾难性回归。
  check('1c 首屏 6 秒内出现（debug 构建，判据不是性能门禁）', firstPage && elapsed < 6000, { ms: elapsed })
  check('1d 标题里的总数是 10000，不是 200', (await countText()) === String(TOTAL), {
    count: await countText(),
  })
  check('1e 底部写明「已显示 200 / 共 10000 条」', (await progressText())?.includes('已显示 200 / 共 10000 条'), {
    progress: await progressText(),
  })

  // ── 2. 加载更多到 400 ──
  await clickSelector('.trash__pager button')
  const secondPage = await waitFor(`document.querySelectorAll('.trash__item').length === 400`, 30000)
  check('2a 点一次「加载更多」到 400 行', secondPage, { rows: await rows() })
  check('2b 计数跟着变成「已显示 400 / 共 10000 条」', (await progressText())?.includes('已显示 400 / 共 10000 条'), {
    progress: await progressText(),
  })

  const before = await origins()
  const uniqueBefore = new Set(before)
  check('2c 已加载的 400 行里没有重复的 origin', uniqueBefore.size === before.length, {
    rows: before.length,
    unique: uniqueBefore.size,
  })
  check(
    '2d 这 400 行正好是磁盘上最新的 400 条（顺序也对）',
    JSON.stringify(before) === JSON.stringify(newest(400)),
    { first: before[0], last: before[before.length - 1] },
  )

  // ── 4. 恢复中间那一页的一条 ──
  const target = originOf(TOTAL - 1 - RESTORE_AT)
  const rect = await ev(`(() => {
    const li = [...document.querySelectorAll('.trash__item')].find(
      (e) => e.querySelector('.trash__origin')?.textContent === ${JSON.stringify(target)},
    )
    if (li === undefined) return null
    li.scrollIntoView({ block: 'center' })
    const b = li.querySelector('button')
    const r = b.getBoundingClientRect()
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
  })()`)
  check('4a 目标行在第二页里（说明分页真的翻过去了）', rect !== null, { target })
  if (rect !== null) await clickAt(rect.x, rect.y)

  const removed = await waitFor(`document.querySelectorAll('.trash__item').length === 399`, 30000)
  check('4b 恢复之后该行从已加载列表里消失（399 行）', removed, { rows: await rows() })
  check('4c 面板**没有**被关掉', await ev(`document.querySelector('.trash-panel') !== null`), {})
  check('4d total 减 1 → 9999', (await countText()) === '9999', { count: await countText() })
  check('4e 面板里出现恢复成功的回执', await ev(`(document.querySelector('.trash__notice')?.textContent ?? '').length > 0`), {
    notice: await ev(`document.querySelector('.trash__notice')?.textContent ?? null`),
  })
  check('4f 文件真的回到了库根', fs.existsSync(path.join(VAULT, path.basename(target))), {
    path: path.basename(target),
  })

  // ── 2'. 恢复之后继续翻：不重不漏 ──
  await clickSelector('.trash__pager button')
  const thirdPage = await waitFor(`document.querySelectorAll('.trash__item').length === 599`, 30000)
  check("2e 恢复之后再点「加载更多」到 599 行", thirdPage, { rows: await rows() })

  const after = await origins()
  const expected = newest(600).filter((origin) => origin !== target)
  const missing = expected.filter((origin) => !after.includes(origin))
  const unexpected = after.filter((origin) => !expected.includes(origin))
  check('2f 恢复一条之后翻页不重不漏（集合相等）', JSON.stringify(after) === JSON.stringify(expected), {
    rows: after.length,
    expected: expected.length,
    missing: missing.slice(0, 3),
    unexpected: unexpected.slice(0, 3),
  })
  check('2g 599 行里没有重复的 origin', new Set(after).size === after.length, {
    rows: after.length,
    unique: new Set(after).size,
  })

  // ── 7. 清空 ──
  await clickSelector('.trash__head .btn--danger')
  const dialog = await waitFor(`document.querySelector('.confirm') !== null`, 5000)
  check('7a 清空有二次确认', dialog, {})
  const purgeStart = Date.now()
  await clickSelector('.confirm__actions .btn--danger')
  const emptied = await waitFor(
    `(document.querySelector('.trash__empty')?.textContent ?? '').includes('回收站是空的')`,
    600000,
  )
  check('7b 清空之后面板显示「回收站是空的」', emptied, {
    ms: Date.now() - purgeStart,
    text: await ev(`document.querySelector('.trash__empty')?.textContent ?? null`),
  })
  check('7c total = 0（标题上的计数消失）', (await countText()) === null, { count: await countText() })
  check('7d 侧栏徽标也归零', (await ev(`document.querySelector('.trash-link__count') === null`)) === true, {})
  const left = fs.existsSync(TRASH) ? fs.readdirSync(TRASH).length : 0
  check('7e 磁盘上一条不剩（删的是全部，不是当前页）', left === 0, { left })

  check('9 全程没有 console 错误', consoleErrors.length === 0, { errors: consoleErrors.slice(0, 5) })
}

run()
  .catch((err) => {
    check('脚本异常', false, { error: err instanceof Error ? err.message : String(err) })
  })
  .finally(() => {
    const failed = results.filter((r) => r.ok !== true)
    log(`\n=== ${results.length - failed.length}/${results.length} 通过 ===`)
    fs.writeFileSync(
      path.join(OUT_DIR, `trash-page-acceptance-${STAMP}.json`),
      JSON.stringify({ results, consoleErrors, stamp: STAMP }, null, 2),
    )
    app?.close()
    killApp()
    process.exit(failed.length === 0 ? 0 : 1)
  })
