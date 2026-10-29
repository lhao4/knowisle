// M7 第十刀（B-09）· 「外部改动不再静默」的端到端验收。真实 WebView2 + CDP。
//
// # 这一条在验什么
//
// 磁盘上的真相与屏幕上的真相分叉时，应用要**知道**，并且让用户**选**。
// 乐观锁（M1）已经保证「不会静默覆盖别人」，但它没保证「用户能看见别人改了
// 什么」：他看到的是一篇过时的文档，敲字、保存、撞冲突，然后面对一个只有
// 「覆盖」的横幅——那一步他无论点什么都 在赌。
//
// # 判据为什么全是「屏幕 vs 磁盘」
//
// 这一刀没有 Rust 逻辑可测（唯一的例外是 `note_rebase`，它在
// `crates/ki-ipc/tests/autosave.rs` 里单独测）。这里要证明的是**用户点得到**：
// 横幅出现、文案对、两个按钮各自做什么、撤销栈有没有被清掉、磁盘一个字节
// 有没有被动过。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`：vite 在 127.0.0.1:1420
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. 应用**刚重启**（`--restart` 自己杀并起）。探针走
//      `addScriptToEvaluateOnNewDocument`，同进程跑第二次会重复注入（B-16）。
//
// 跑法：`node scripts/e2e/cdp-external-change.mjs --restart`
// 退出码只有三种（B-19）：0 全跑全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'

import { openApp, killApp, MOD } from './cdp.mjs'
import { EXTERNAL_VAULT as VAULT, OUT_DIR, sleep } from './env.mjs'
import { resetVault } from './fixtures.mjs'

const STAMP = String(Date.now()).slice(-6)

/* ───────────────────────── 夹具 ───────────────────────── */

const A = '# 甲\n\n第一版。\n'
const A_EXTERNAL = '# 甲\n\n外部改过的正文。\n'
const B = '# 乙\n\n另一篇。\n'
const B_EXTERNAL = '# 乙\n\n对方改的。\n'
const DIRTY = '# 脏\n\n起点。\n'
const DIRTY_EXTERNAL = '# 脏\n\n外部覆盖。\n'
const ENC = '# 编码\n\n这里会变成 GBK。\n'
/** GBK 的「# 编码验收\n中文笔记\n」（字节由 `iconv -f UTF-8 -t GBK` 生成）。 */
const GBK_TEXT = '# 编码验收\n中文笔记\n'
const GBK_HEX = '2320b1e0c2ebd1e9cad50ad6d0cec4b1cabcc70a'
const SAME = '# 同\n\n两边一模一样。\n'
const GONE = '# 没了\n\n马上就要被删掉。\n'

const fileOf = (name) => path.join(VAULT, name)
const disk = (name) => fs.readFileSync(fileOf(name))

const buildFixture = () => {
  resetVault(
    VAULT,
    {
      'a.md': A,
      'b.md': B,
      'dirty.md': DIRTY,
      'enc.md': ENC,
      'same.md': SAME,
      'gone.md': GONE,
    },
    { clearTrash: true },
  )
}

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  console.log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}
const skip = (name, reason) => {
  results.push({ name, ok: null, detail: reason })
  console.log(`SKIP  ${name}  ${reason}`)
}

/* ───────────────────────── 会话 ───────────────────────── */

const consoleErrors = []
const session = await openApp({ vault: VAULT, prepare: buildFixture, errors: consoleErrors })
const { ev, waitFor } = session
const send = (method, params) => session.send(method, params)
const press = (key, code, vk, modifiers = 0) => session.press(key, code, vk, modifiers)

await sleep(300)

const clickText = async (selector, text, settle = 220) => {
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
      await session.clickAt(rect.x, rect.y, settle)
      return true
    }
    await sleep(150)
  }
  return false
}

/**
 * 编辑器里的正文。
 *
 * **不能**用 `.cm-content` 的 `textContent`：CM6 把每一行渲染成一个
 * `.cm-line` div，`textContent` 会把它们**无分隔**地拼起来（`# 甲第一版。`）。
 * 按行取再 `join('\n')` 才是文档内容，也才能和磁盘上的字节做等值断言。
 */
const editorText = () =>
  ev(`[...document.querySelectorAll('.cm-content .cm-line')].map((l) => l.textContent).join('\\n')`)

const saveLabel = () => ev(`document.querySelector('.viewer__meta')?.textContent ?? null`)

/**
 * 两段正文是不是同一篇。
 *
 * 编辑器侧按行拼接（末尾那个空行会带出一个 `\n`），磁盘侧是完整字节。
 * 只抹掉行尾差异，**不折叠中间的空行**——那正是 markdown 的语义所在。
 */
const sameText = (actual, expected) =>
  (actual ?? '').replace(/\r\n/g, '\n').trimEnd() === expected.replace(/\r\n/g, '\n').trimEnd()

/** 外部改动那条横幅（与「用我的版本覆盖」那条同属 `.banner--warn`，按文案区分）。 */
const externalBanner = () =>
  ev(`(() => {
    const b = [...document.querySelectorAll('.banner')]
      .find((x) => x.textContent.includes('外部') || x.textContent.includes('不在磁盘上'))
    if (!b) return null
    return {
      text: b.textContent.trim(),
      buttons: [...b.querySelectorAll('button')].map((x) => x.textContent.trim()),
    }
  })()`)

/** 派发 window focus。`Page.bringToFront` 不会可靠地派发它（TOOLING）。 */
const dispatchFocus = async () => {
  await ev(`window.dispatchEvent(new Event('focus')); true`)
  await sleep(60)
}

/** 打开一篇根目录下的笔记，等编辑器真的把它渲染出来。 */
const openNote = async (name) => {
  const clicked = await clickText('.tree-row', name, 260)
  if (!clicked) return false
  const ok = await waitFor(
    `document.querySelector('.breadcrumb')?.textContent?.includes(${JSON.stringify(name)}) === true`,
    5000,
  )
  if (!ok) return false
  return waitFor(`document.querySelector('.cm-content .cm-line') !== null`, 5000)
}

/** 光标送到文末再打字。 */
const typeAtEnd = async (text) => {
  await ev(`document.querySelector('.cm-content').focus()`)
  await press('End', 'End', 35, MOD.ctrl)
  await session.insertText(text)
}

/** 轮询磁盘，直到 `predicate(buffer)` 为真。 */
const waitDisk = async (name, predicate, timeoutMs = 8000) => {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    if (predicate(disk(name))) return true
    await sleep(120)
  }
  return false
}

const waitBanner = async (timeoutMs = 4000) => {
  const appeared = await waitFor(
    `[...document.querySelectorAll('.banner')].some((x) => x.textContent.includes('外部') || x.textContent.includes('不在磁盘上'))`,
    timeoutMs,
  )
  return appeared ? await externalBanner() : null
}

/** 磁盘写入与上一次读之间的 mtime 必须真的变——否则这一条什么都没验。 */
const externalWrite = async (name, content) => {
  await sleep(30)
  fs.writeFileSync(fileOf(name), content, 'utf8')
}

/* ═══════════════════════ 1–2. 外部修改 → 横幅 → 重新加载 ═══════════════════════ */

await openNote('a.md')
const aBefore = await editorText()
await externalWrite('a.md', A_EXTERNAL)
await dispatchFocus()
const banner1 = await waitBanner()
check(
  '1 外部改内容 + focus → 横幅出现（含「外部」与文件名，两个按钮）',
  banner1 !== null &&
    banner1.text.includes('外部') &&
    banner1.text.includes('a.md') &&
    banner1.buttons.length === 2 &&
    banner1.buttons.includes('重新加载') &&
    banner1.buttons.includes('保留我的'),
  { banner: banner1, before: aBefore },
)

const reloadClicked = await clickText('.banner button', '重新加载', 350)
const contentReloaded = await waitFor(
  `[...document.querySelectorAll('.cm-content .cm-line')].map((l) => l.textContent).join('\\n').trimEnd() === ${JSON.stringify(A_EXTERNAL.trimEnd())}`,
  5000,
)
const labelAfterReload = await saveLabel()
const noConflict = await ev(
  `![...document.querySelectorAll('.banner')].some((x) => x.textContent.includes('用我的版本覆盖'))`,
)
// 撤销栈必须被清空：Ctrl+Z 既不能把旧内容找回来，也不能把它写回磁盘。
await ev(`document.querySelector('.cm-content').focus()`)
await press('z', 'KeyZ', 90, MOD.ctrl)
await sleep(600)
const afterUndo = await editorText()
const diskAfterUndo = await waitDisk('a.md', (buf) => buf.toString('utf8') === A_EXTERNAL, 3000)
check(
  '2 重新加载 → 内容 == 磁盘、顶栏「已保存」、无冲突、撤销栈已清空',
  reloadClicked &&
    contentReloaded &&
    (labelAfterReload ?? '').startsWith('已保存') &&
    noConflict &&
    sameText(afterUndo, A_EXTERNAL) &&
    diskAfterUndo,
  { reloadClicked, contentReloaded, label: labelAfterReload, noConflict, afterUndo, diskAfterUndo },
)

/* ═══════════════════════ 3. 保留我的：不再重复问 ═══════════════════════ */

await openNote('b.md')
const bBefore = await editorText()
await externalWrite('b.md', B_EXTERNAL)
await dispatchFocus()
const banner2 = await waitBanner()
const kept = await clickText('.banner button', '保留我的', 250)
const bannerGone = await waitFor(
  `![...document.querySelectorAll('.banner')].some((x) => x.textContent.includes('外部'))`,
  2000,
)
const bAfter = await editorText()
await dispatchFocus()
await sleep(700)
const noRepeat = await ev(
  `![...document.querySelectorAll('.banner')].some((x) => x.textContent.includes('外部'))`,
)
check(
  '3 保留我的 → 内容不变、横幅消失；再次 focus 不再弹（同一篇同一版只问一次）',
  banner2 !== null && kept && bannerGone && sameText(bAfter, bBefore) && noRepeat,
  { banner: banner2?.text?.slice(0, 40), kept, bannerGone, unchanged: sameText(bAfter, bBefore), noRepeat },
)

/*
 * 3b. 「保留我的」之后继续写：保存撞冲突，「用我的版本覆盖」仍然有效。
 *
 * 这一条同时是收尾——不把分歧解决掉，下一篇切不过去（`openNote` 的 flush
 * 会撞同一个冲突，按既有语义它拒绝切换）。它也顺带锁住「既有按钮语义没被
 * 这一刀改坏」。
 */
await typeAtEnd('！')
const conflictShown = await waitFor(
  `[...document.querySelectorAll('.banner')].some((x) => x.textContent.includes('用我的版本覆盖'))`,
  10000,
)
const overrode = conflictShown && (await clickText('.banner button', '用我的版本覆盖', 400))
const bSaved = await waitDisk('b.md', (buf) => buf.toString('utf8').includes('！'), 8000)
check(
  '3b 保留我的之后再编辑 → 撞冲突 → 「用我的版本覆盖」仍然有效（既有按钮语义未动）',
  conflictShown && overrode && bSaved,
  { conflictShown, overrode, bSaved },
)

/* ═══════════════════════ 4. 未保存时的外部改动：文案 + 二次确认 ═══════════════════════ */

await openNote('dirty.md')
// 第一笔立刻落盘（首次编辑不节流），第二笔才会压在 1.5 秒的槽位里。
await typeAtEnd('一')
await waitFor(`document.querySelector('.viewer__meta')?.textContent?.startsWith('已保存') === true`, 8000)
await typeAtEnd('二')
await sleep(400)
await externalWrite('dirty.md', DIRTY_EXTERNAL)
await dispatchFocus()
const banner3 = await waitBanner()
const dirtyCopy = banner3 !== null && banner3.text.includes('你的改动还没保存')

const reopen = await clickText('.banner button', '重新加载', 250)
const dialogShown = await waitFor(`document.querySelector('[role="alertdialog"]') !== null`, 2500)
const dialogText = await ev(`document.querySelector('[role="alertdialog"]')?.textContent ?? null`)
const cancelled = await clickText('.confirm__actions button', '取消', 250)
const dialogClosed = await waitFor(`document.querySelector('[role="alertdialog"]') === null`, 2000)
const stillMine = await editorText()
const diskStillExternal = Buffer.compare(disk('dirty.md'), Buffer.from(DIRTY_EXTERNAL, 'utf8')) === 0
check(
  '4 有未保存改动时：文案说明「你的改动还没保存」，重新加载要二次确认，取消后两边都不动',
  banner3 !== null &&
    dirtyCopy &&
    reopen &&
    dialogShown &&
    (dialogText ?? '').includes('丢弃') &&
    cancelled &&
    dialogClosed &&
    stillMine.includes('二') &&
    diskStillExternal,
  { banner: banner3?.text, dialogShown, dialog: (dialogText ?? '').slice(0, 60), cancelled, dialogClosed, stillMine, diskStillExternal },
)

// 收尾：真的丢弃并重新加载，把状态弄干净（否则下一篇切不过去——flush 会撞冲突）。
await clickText('.banner button', '重新加载', 250)
await waitFor(`document.querySelector('[role="alertdialog"]') !== null`, 2500)
const confirmed = await clickText('.confirm__actions button', '丢弃并重新加载', 350)
const dirtyReloaded = await waitFor(
  `[...document.querySelectorAll('.cm-content .cm-line')].map((l) => l.textContent).join('\\n').trimEnd() === ${JSON.stringify(DIRTY_EXTERNAL.trimEnd())}`,
  5000,
)
check(
  '4b 确认之后 → 屏幕换成磁盘那一版，刚敲的字被丢弃',
  confirmed && dirtyReloaded,
  { confirmed, dirtyReloaded },
)

/* ═══════════════════════ 5. 外部改成 GBK：重新加载后解码正确 ═══════════════════════ */

await openNote('enc.md')
await sleep(30)
// 必须写 Buffer：写字符串就是 UTF-8，那测的就不是 GBK 了。
fs.writeFileSync(fileOf('enc.md'), Buffer.from(GBK_HEX, 'hex'))
await dispatchFocus()
const banner4 = await waitBanner()
await clickText('.banner button', '重新加载', 400)
const gbkText = await waitFor(
  `[...document.querySelectorAll('.cm-content .cm-line')].map((l) => l.textContent).join('\\n').trimEnd() === ${JSON.stringify(GBK_TEXT.trimEnd())}`,
  6000,
)
const encodingRow = await ev(`document.querySelector('.viewer__encoding')?.textContent?.trim() ?? null`)
check(
  '5 外部把文件改成 GBK → 重新加载后正文正确、提示行「已按 GBK 解码」出现',
  banner4 !== null && gbkText && (encodingRow ?? '').includes('已按 GBK 解码'),
  { banner: banner4 !== null, gbkText, row: encodingRow },
)

/* ═══════════════════════ 6. 内容相同、mtime 变了：不误报，且保存不冲突 ═══════════════════════ */

await openNote('same.md')
const sameBefore = await editorText()
await externalWrite('same.md', SAME)
await dispatchFocus()
await sleep(700)
const noBannerForSame = await ev(
  `![...document.querySelectorAll('.banner')].some((x) => x.textContent.includes('外部'))`,
)
// 基线有没有真的推进，只能由「下一次保存成不成」来证。
await typeAtEnd('✓')
const sameSaved = await waitDisk('same.md', (buf) => buf.toString('utf8').includes('✓'), 10000)
const sameLabel = await waitFor(
  `document.querySelector('.viewer__meta')?.textContent?.startsWith('已保存') === true`,
  10000,
)
const noConflictAfterSame = await ev(
  `![...document.querySelectorAll('.banner')].some((x) => x.textContent.includes('用我的版本覆盖'))`,
)
check(
  '6 外部写成完全相同的内容 → 不弹横幅（不误报），下一次保存不冲突（adopt-baseline 生效）',
  sameText(sameBefore, SAME) && noBannerForSame && sameSaved && sameLabel && noConflictAfterSame,
  { sameBefore, noBannerForSame, sameSaved, sameLabel, noConflictAfterSame },
)

/* ═══════════════════════ 7. 外部删除：不崩、能继续编辑 ═══════════════════════ */

await openNote('gone.md')
await sleep(30)
fs.unlinkSync(fileOf('gone.md'))
await dispatchFocus()
const banner5 = await waitBanner()
const deletedCopy = banner5 !== null && banner5.text.includes('不在磁盘上')
// 「能继续编辑」= 编辑器还在、字进得去、应用没崩（consoleErrors 在最后一条判）。
await typeAtEnd('还在写')
await sleep(2200)
const stillEditable = await ev(`(() => {
  const el = document.querySelector('.cm-content')
  if (el === null) return null
  return { text: el.textContent, breadcrumb: document.querySelector('.breadcrumb')?.textContent ?? null }
})()`)
check(
  '7 外部删除 → 横幅说「不在磁盘上」，应用不崩、能继续编辑',
  banner5 !== null && deletedCopy && stillEditable !== null && stillEditable.text.includes('还在写') && (stillEditable.breadcrumb ?? '').includes('gone.md'),
  { banner: banner5?.text, deletedCopy, stillEditable },
)

/* ═══════════════════════ 8. 无 console error ═══════════════════════ */

check('8 运行期无 console error / 未捕获异常', consoleErrors.length === 0, consoleErrors.slice(0, 5))

/* ───────────────────────── 收尾 ───────────────────────── */

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
console.log(
  `\n${failed.length === 0 ? '✅' : '❌'} 共 ${results.length} 项：${results.length - failed.length - skipped.length} 通过 / ${failed.length} 失败 / ${skipped.length} 跳过`,
)
fs.writeFileSync(
  path.join(OUT_DIR, `external-change-${STAMP}.json`),
  JSON.stringify({ results, consoleErrors, stamp: STAMP }, null, 2),
)
session.close()
killApp()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
