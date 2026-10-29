// M7 第九刀（B-38）· 「转存为 UTF-8」的端到端验收。真实 WebView2 + CDP。
//
// # 这一条在验什么
//
// 第八刀只证明了 Rust 侧对：编码探测、保存写回原编码、装不下就拒绝。
// 它**没有**证明「用户点得到」——GUI 层对编码路径的覆盖是零。这一刀补的
// 就是那一段：那行「已按 GBK 解码」上多了一个按钮，按钮后面有二次确认，
// 确认之后磁盘上的字节真的变了，而且原文件躺在 `.knowisle/history/` 里。
//
// # 判据一律看磁盘字节
//
// `Buffer.compare(fs.readFileSync(p), Buffer.from(text, 'utf8')) === 0`，
// 不是 `buf.toString('utf8') === text`：后者对 GBK 字节也会「成功」解出
// 一堆 U+FFFD，看起来像通过了。夹具也用 `Buffer` 写——写 `'中文笔记'`
// 是 UTF-8，那测的就不是 GBK 了。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`：vite 在 127.0.0.1:1420
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. 应用**刚重启**（`--restart` 自己杀并起）。探针走
//      `addScriptToEvaluateOnNewDocument`，同进程跑第二次会重复注入（B-16）。
//
// 跑法：`node scripts/e2e/cdp-encoding.mjs --restart`
// 退出码只有三种（B-19）：0 全跑全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'

import { openApp, killApp, MOD } from './cdp.mjs'
import { ENCODING_VAULT as VAULT, OUT_DIR, sleep } from './env.mjs'
import { resetVault } from './fixtures.mjs'

const STAMP = String(Date.now()).slice(-6)

/** GBK 的「# 编码验收\n中文笔记\n」。字节由 `iconv -f UTF-8 -t GBK` 生成。 */
const GBK_TEXT = '# 编码验收\n中文笔记\n'
const GBK_HEX = '2320b1e0c2ebd1e9cad50ad6d0cec4b1cabcc70a'

/** Big5 的「中文筆記」+ 重复映射的「═」（`A2 A4` 与 `F9 F9` 同字）。 */
const BIG5_TEXT = '中文筆記═'
const BIG5_HEX = 'a4a4a4e5b5a7b04fa2a4'

const UTF8_TEXT = '# 普通 UTF-8\n不需要转存。\n'

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  console.log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}
const skip = (name, reason) => {
  results.push({ name, ok: null, detail: reason })
  console.log(`SKIP  ${name}  ${reason}`)
}

/* ───────────────────────── 夹具 ───────────────────────── */

const fileOf = (name) => path.join(VAULT, name)
const disk = (name) => fs.readFileSync(fileOf(name))

/**
 * 四篇笔记：纯 GBK、GBK（要加 emoji）、Big5（重复映射）、UTF-8。
 *
 * `resetVault` 只写 UTF-8 字符串，所以 GBK / Big5 两篇在这里用 `Buffer` 覆盖。
 * 它也**不删** `.knowisle/`（应用开着时索引库被占，Windows 上整目录删会
 * EBUSY，B-32），于是上一轮转存留下的备份会活到这一轮——判据因此写成
 * 「本轮没有新增备份」，而不是「目录为空」。
 */
const buildFixture = () => {
  resetVault(VAULT, { 'utf8.md': UTF8_TEXT }, { clearTrash: true })
  fs.writeFileSync(fileOf('gbk.md'), Buffer.from(GBK_HEX, 'hex'))
  fs.writeFileSync(fileOf('gbk-emoji.md'), Buffer.from(GBK_HEX, 'hex'))
  fs.writeFileSync(fileOf('big5.md'), Buffer.from(BIG5_HEX, 'hex'))
}

/** 某篇笔记现有的全部备份（`history/<rel>/` 下的文件名）。 */
const backupsOf = (rel) => {
  const dir = path.join(VAULT, '.knowisle', 'history', rel)
  try {
    return fs.readdirSync(dir).sort()
  } catch {
    return []
  }
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

bind(await openApp({ vault: VAULT, prepare: buildFixture, errors: consoleErrors }))
const send = (method, params) => app.send(method, params)
const press = (key, code, vk, modifiers = 0) => app.press(key, code, vk, modifiers)
const insertText = (text) => app.insertText(text)

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
  await sleep(140)
}

/**
 * 点一个按文字找的元素（按钮、确认框里的动作）。
 *
 * 坐标**每次重新取**：树在重扫期间会长高一行，先算后点会点到隔壁
 * （B-33 里踩过，症状是「点了没反应」）。
 */
const clickText = async (selector, text, settle = 200) => {
  for (let i = 0; i < 20; i += 1) {
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
    await sleep(150)
  }
  return false
}

/** 打开一篇根目录下的笔记，等 breadcrumb 里出现它的名字。 */
const openNote = async (name) => {
  // 树的 label 就是文件名（含 `.md`），所以这里必须**整串**比对：
  // `gbk.md` 与 `gbk-emoji.md` 用 includes 会点错。
  const clicked = await clickText('.tree-row', name, 250)
  if (!clicked) return false
  return waitFor(
    `document.querySelector('.breadcrumb')?.textContent?.includes(${JSON.stringify(name)}) === true`,
    5000,
  )
}

/** 顶部那行「已按 X 解码」的现状。 */
const encodingRow = () =>
  ev(`(() => {
    const row = document.querySelector('.viewer__encoding')
    if (!row) return { present: false }
    const action = row.querySelector('.viewer__encoding-action')
    return { present: true, text: row.textContent.trim(), action: action?.textContent.trim() ?? null }
  })()`)

const saveLabel = () => ev(`document.querySelector('.viewer__meta')?.textContent ?? null`)

/** 轮询磁盘，直到 `predicate(buffer)` 为真。返回是否等到。 */
const waitDisk = async (name, predicate, timeoutMs = 8000) => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (predicate(disk(name))) return true
    await sleep(120)
  }
  return false
}

/** 光标送到文末再打字。 */
const typeAtEnd = async (text) => {
  await ev(`document.querySelector('.cm-content').focus()`)
  await press('End', 'End', 35, MOD.ctrl)
  await insertText(text)
}

/* ═══════════════════════ 1. 夹具就位 ═══════════════════════ */

const treeTitles = await ev(
  `[...document.querySelectorAll('.tree-row')].map((e) => e.title).sort()`,
)
check(
  '1 四篇夹具都在树里（gbk / gbk-emoji / big5 / utf8）',
  Array.isArray(treeTitles) &&
    ['big5.md', 'gbk-emoji.md', 'gbk.md', 'utf8.md'].every((n) => treeTitles.includes(n)),
  treeTitles,
)

/* ═══════════════════════ 2. 提示行 + 入口 ═══════════════════════ */

const opened = await openNote('gbk.md')
const row = await encodingRow()
check(
  '2 打开 GBK 笔记：提示行出现，带「转存为 UTF-8」按钮',
  opened && row.present === true && row.text.includes('已按 GBK 解码') && row.action === '转存为 UTF-8',
  { opened, row },
)

/* ═══════════════════════ 3. 病灶：加 emoji 存不了 ═══════════════════════ */

await openNote('gbk-emoji.md')
await typeAtEnd('😀')
// 自动保存 1.5 秒一拍，写失败之后顶栏变成「保存失败」。
const rejected = await waitFor(`document.querySelector('.viewer__meta')?.textContent === '保存失败'`, 10000)
const gbkBefore = Buffer.from(GBK_HEX, 'hex')
check(
  '3 GBK 里加 emoji → 保存被拒（顶栏「保存失败」，磁盘一个字节都没动）',
  rejected && Buffer.compare(disk('gbk-emoji.md'), gbkBefore) === 0,
  { rejected, label: await saveLabel(), size: disk('gbk-emoji.md').length },
)

/* ═══════════════════════ 4–5. 二次确认：能取消 ═══════════════════════ */

const emojiRow = await encodingRow()
const clicked = await clickText('.viewer__encoding-action', '转存为 UTF-8')
const dialog = await waitFor(`document.querySelector('[role="alertdialog"]') !== null`, 2000)
const dialogText = await ev(`document.querySelector('[role="alertdialog"]')?.textContent ?? null`)
check(
  '4 点按钮 → 二次确认（role=alertdialog，文案写明备份到 .knowisle/history/）',
  emojiRow.present === true && clicked && dialog && (dialogText ?? '').includes('.knowisle/history/'),
  { dialog, text: (dialogText ?? '').slice(0, 120) },
)

const cancelled = await clickText('.confirm__actions button', '取消')
const stillGbk = await waitDisk('gbk-emoji.md', (buf) => Buffer.compare(buf, gbkBefore) === 0, 500)
const rowAfterCancel = await encodingRow()
const dialogClosed = await waitFor(`document.querySelector('[role="alertdialog"]') === null`, 2000)
check(
  '5 点「取消」→ 磁盘仍是 GBK，提示行还在（没有确认就不许动字节）',
  cancelled && stillGbk && rowAfterCancel.present === true && dialogClosed,
  { cancelled, dialogClosed, row: rowAfterCancel },
)

/* ═══════════════════════ 6–8. 转存：提示行消失 / 磁盘 UTF-8 / 备份 ═══════════════════════ */

// 取消之后浮层已经关掉了，确认框要**重新打开**——这一步漏了的话，
// `clickText('.confirm__actions button', '转存')` 会点到一个不存在的按钮，
// 后面四条全部连坐（本脚本第一版就是这么红的）。
const reopened = await clickText('.viewer__encoding-action', '转存为 UTF-8')
await waitFor(`document.querySelector('[role="alertdialog"]') !== null`, 2000)

const backupsBefore = backupsOf('gbk-emoji.md')
const confirmed = await clickText('.confirm__actions button', '转存', 300)
const rowGone = await waitFor(`document.querySelector('.viewer__encoding') === null`, 8000)

// 转存之后编辑器里压着的那个 emoji 会立刻被冲下去（磁盘已经是 UTF-8 了），
// 所以期望字节是「原正文 + 😀」。
const wantEmoji = Buffer.from(`${GBK_TEXT}😀`, 'utf8')
const emojiSaved = await waitDisk('gbk-emoji.md', (buf) => Buffer.compare(buf, wantEmoji) === 0)

check(
  '6 确认转存 → 提示行消失（decodedFrom 变 null，e2e 判据是元素不存在）',
  confirmed && rowGone,
  { confirmed, rowGone, label: await saveLabel() },
)
check(
  '7 磁盘字节 == UTF-8（Buffer.compare，不是 toString 比对）',
  emojiSaved && Buffer.compare(disk('gbk-emoji.md'), wantEmoji) === 0,
  { emojiSaved, size: disk('gbk-emoji.md').length, want: wantEmoji.length },
)

const backupsAfter = backupsOf('gbk-emoji.md')
const added = backupsAfter.filter((name) => !backupsBefore.includes(name))
const backupBytes = added.length === 1 ? fs.readFileSync(path.join(VAULT, '.knowisle', 'history', 'gbk-emoji.md', added[0])) : null
check(
  '8 备份逐字节等于原文件（本轮新增恰好一份，可撤销的唯一保证）',
  added.length === 1 && backupBytes !== null && Buffer.compare(backupBytes, gbkBefore) === 0,
  { before: backupsBefore.length, after: backupsAfter.length, added, match: backupBytes ? Buffer.compare(backupBytes, gbkBefore) === 0 : null },
)

/* ═══════════════════════ 9. 转存之后 emoji 存得下去 ═══════════════════════ */

// 7 已经证明了字节；这一条证明的是**保存链路**也通了：再打一个字仍然能存。
const marker = `KI-ENC-${STAMP}`
await typeAtEnd(`\n${marker}\n`)
const savedAgain = await waitDisk('gbk-emoji.md', (buf) => buf.includes(Buffer.from(marker, 'utf8')), 10000)
// 磁盘落了不等于顶栏落了：`noteStatus` 是在写盘**之后**才回来的一次 IPC，
// 采样式断言会稳定地读到「保存中…」。这里必须等它自己翻成「已保存」。
const labelSettled = await waitFor(
  `document.querySelector('.viewer__meta')?.textContent?.startsWith('已保存') === true`,
  10000,
)
check(
  '9 转存之后再编辑仍能保存（保存链路没有被转存弄坏）',
  savedAgain && labelSettled,
  { savedAgain, labelSettled, label: await saveLabel() },
)

/* ═══════════════════════ 10. Big5 重复映射：转存后能改字 ═══════════════════════ */

await openNote('big5.md')
const big5Row = await encodingRow()
const big5BackupsBefore = backupsOf('big5.md')
const big5Rejected = await (async () => {
  // 转存之前：任何保存都被拒。打一个字然后等「保存失败」。
  await typeAtEnd('。')
  return waitFor(`document.querySelector('.viewer__meta')?.textContent === '保存失败'`, 10000)
})()
const big5Before = Buffer.from(BIG5_HEX, 'hex')
const big5Untouched = Buffer.compare(disk('big5.md'), big5Before) === 0

await clickText('.viewer__encoding-action', '转存为 UTF-8')
await clickText('.confirm__actions button', '转存', 300)
const big5RowGone = await waitFor(`document.querySelector('.viewer__encoding') === null`, 8000)
const big5Want = Buffer.from(`${BIG5_TEXT}。`, 'utf8')
const big5Saved = await waitDisk('big5.md', (buf) => Buffer.compare(buf, big5Want) === 0)
const big5BackupsAfter = backupsOf('big5.md')
check(
  '10 Big5 重复映射：转存前保存被拒且磁盘不动 → 转存后改字能存，磁盘是 UTF-8',
  big5Row.present === true &&
    big5Rejected &&
    big5Untouched &&
    big5RowGone &&
    big5Saved &&
    big5BackupsAfter.length === big5BackupsBefore.length + 1,
  {
    before: big5Row,
    rejected: big5Rejected,
    untouched: big5Untouched,
    gone: big5RowGone,
    saved: big5Saved,
    backups: `${big5BackupsBefore.length} → ${big5BackupsAfter.length}`,
  },
)

/* ═══════════════════════ 11. UTF-8 笔记没有入口 ═══════════════════════ */

await openNote('utf8.md')
const utf8Row = await encodingRow()
check(
  '11 本来就是 UTF-8 的笔记：没有提示行，也就没有转存入口（幂等，不出无意义的动作）',
  utf8Row.present === false,
  utf8Row,
)

/* ═══════════════════════ 12. 无 console error ═══════════════════════ */

check('12 运行期无 console error / 未捕获异常', consoleErrors.length === 0, consoleErrors.slice(0, 5))

/* ───────────────────────── 收尾 ───────────────────────── */

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
console.log(
  `\n${failed.length === 0 ? '✅' : '❌'} 共 ${results.length} 项：${results.length - failed.length - skipped.length} 通过 / ${failed.length} 失败 / ${skipped.length} 跳过`,
)
fs.writeFileSync(
  path.join(OUT_DIR, `encoding-acceptance-${STAMP}.json`),
  JSON.stringify({ results, consoleErrors, stamp: STAMP }, null, 2),
)
app.close()
killApp()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
