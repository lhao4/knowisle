// M7 第十三刀（B-05）· 忽略规则可配置的端到端验收。
//
// # 这一条在验什么
//
// 「我的某个文件夹不见了」——在它之前，这个问题的答案是「什么都没有」：
// 目录被剪枝剪掉了，连 `skipped` 都不计，因为扫描器根本没访问过它。
//
// 六条判据（编号对齐施工指令 §4）：
//
//   1. 默认树里 logseq / dist / node_modules / my-notes/dist 都不在，且
//      「跳过 N」不出现（忽略不是跳过）；
//   2. 侧栏常驻「忽略规则」入口 → 面板列出 15 条内置规则与配置文件路径；
//   3. `extra` + `negate` 之后：logseq 回来了、my-notes 消失了，面板标出
//      「已停用」与「额外忽略」；
//   4. 配置写坏（`extra = "dist"`）：树回到默认 + 面板给出「第 2 行」+ 人话，
//      侧栏入口挂「配置有误」；
//   5. `negate = [".git"]`：`.git` 仍在树外 + 一句点名它的拒绝理由；
//   6. 删掉配置文件：树回到默认。
//
// # 为什么每条都要 reload
//
// 应用不监听文件变化（ADR-011 已否过 watcher）。配置只在 `vault_scan` 开头读
// 一次，所以「改完要重扫」不是偷懒，是契约——这条也写在面板上。
//
// 前置（缺一条都不算数）：
//   1. `pnpm --filter @knowisle/desktop dev`（只有 vite，别用 `pnpm dev`）
//   2. `cargo build -p knowisle`（否则测的是旧二进制）
//   3. 应用**刚重启**（`--restart` 自己杀并起）
//
// 跑法：`node scripts/e2e/cdp-ignore-rules.mjs --restart`
// 退出码三种（B-19）：0 全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'

import { killApp, openApp } from './cdp.mjs'
import { IGNORE_VAULT as VAULT, OUT_DIR, sleep } from './env.mjs'
import { assertTempVault, resetVault } from './fixtures.mjs'

const CONFIG = path.join(VAULT, '.knowisle', 'config.toml')

const results = []
const check = (name, ok, detail) => {
  results.push({ name, ok: ok === true, detail })
  console.log(`${ok === true ? 'PASS' : 'FAIL'}  ${name}  ${JSON.stringify(detail)}`)
}

const abs = (rel) => path.join(VAULT, rel)

/**
 * 删掉配置文件。
 *
 * **必须显式删**：`resetVault` 的 `sweepNotes` 只扫 `.md` 且跳过 `.knowisle`，
 * 上一轮写进去的配置会跨用例活下来（§6 的已知坑）。脚本开头与结尾各删一次。
 */
function clearConfig() {
  fs.rmSync(CONFIG, { force: true })
}

function writeConfig(text) {
  fs.mkdirSync(path.dirname(CONFIG), { recursive: true })
  fs.writeFileSync(CONFIG, text, 'utf8')
}

/**
 * 夹具：四个「会被内置名单剪掉」的位置 + 两篇正常笔记。
 *
 * `my-notes/dist` 是这一刀的真痛点：它嵌在用户自己的目录里，按名字匹配，
 * 一样被吞——而用户根本想不到是「dist 这个名字」的问题。
 */
function buildFixture() {
  clearConfig()
  resetVault(
    VAULT,
    {
      'notes/a.md': '# a\n',
      'notes/b.md': '# b\n',
      'my-notes/keep.md': '# keep\n',
      'my-notes/dist/z.md': '# 用户的 dist\n',
      'logseq/pages/a.md': '# logseq page\n',
      'dist/x.md': '# 构建产物\n',
      'node_modules/y.md': '# 别人的 README\n',
      '.git/config.md': '# git\n',
    },
    { clearTrash: true },
  )
}

assertTempVault(VAULT)

const consoleErrors = []
let app
let ev
let waitFor

const session = await openApp({
  vault: VAULT,
  prepare: () => buildFixture(),
  errors: consoleErrors,
})
app = session
ev = session.ev
waitFor = session.waitFor
await sleep(400)

/* ───────────────────────── 读屏 ───────────────────────── */

/** 树里所有节点的路径（`title` 是完整 POSIX 路径，目录与文件都有）。 */
const readTree = () =>
  ev(`[...document.querySelectorAll('.tree-row')].map((row) => row.getAttribute('title'))`)

/** 侧栏底部那个常驻入口的文本。 */
const ignoreLink = () =>
  ev(`document.querySelector('.ignore-link')?.textContent ?? null`)

/**
 * 展开一个目录。
 *
 * 树的自动展开只发生在**第一层**（`TreeNode` 里 `depth === 0` 才展开），
 * 所以 `logseq/pages/a.md` 这种两层深的文件默认不在 DOM 里。不点开就断言
 * 「它出现了」只能靠 `title` 不存在——那证明不了任何事。
 */
async function expandRow(title) {
  const clicked = await ev(`(() => {
    const row = [...document.querySelectorAll('.tree-row')]
      .find((el) => el.getAttribute('title') === ${JSON.stringify(title)})
    if (row === undefined) return false
    row.click()
    return true
  })()`)
  await sleep(120)
  return clicked === true
}

/** 面板内容。不在时返回 null。 */
const readPanel = () =>
  ev(`(() => {
    const panel = document.querySelector('.ignore-panel')
    if (panel === null) return null
    const section = (label) => panel.querySelector('[aria-label="' + label + '"]')
    const rules = (label) =>
      [...(section(label)?.querySelectorAll('.ignore__rule') ?? [])].map((li) => ({
        name: li.querySelector('code')?.textContent ?? '',
        flags: [...li.querySelectorAll('.ignore__flag')].map((f) => f.textContent ?? ''),
        disabled: li.classList.contains('is-disabled'),
      }))
    return {
      error: panel.querySelector('.ignore__error-line')?.textContent ?? null,
      errorPath: panel.querySelector('.ignore__error-path')?.textContent ?? null,
      errorNote: panel.querySelector('.ignore__error-note')?.textContent ?? null,
      configPath: panel.querySelector('.ignore__path')?.textContent ?? null,
      builtin: rules('内置忽略名单'),
      user: rules('用户规则列表'),
      headings: [...panel.querySelectorAll('.skip__heading')].map((h) => h.textContent ?? ''),
      empty: panel.querySelector('.trash__empty')?.textContent ?? null,
    }
  })()`)

async function openPanel() {
  if (!(await app.clickSelector('.ignore-link'))) return null
  if (!(await waitFor(`document.querySelector('.ignore-panel') !== null`, 3000))) return null
  return readPanel()
}

async function closePanel() {
  await app.press('Escape', 'Escape', 27)
  await waitFor(`document.querySelector('.ignore-panel') === null`, 2000)
}

async function reload() {
  await app.reload()
  await waitFor(`document.querySelector('.tree-row') !== null`, 30000)
  await sleep(300)
}

/* ═══════════════════════ 1. 默认树：四个都不在 ═══════════════════════ */

const defaults = (await readTree()) ?? []
check(
  '1.default-keeps-real-notes',
  defaults.includes('notes/a.md') && defaults.includes('notes/b.md') && defaults.includes('my-notes/keep.md'),
  { tree: defaults },
)
check(
  '1.builtin-names-are-pruned',
  // 断言**目录行**而不是深层文件：`my-notes` 是自动展开的第一层，所以
  // `my-notes/dist` 这个目录行在不在，是一个真实可见的事实。
  ['logseq', 'dist', 'node_modules', 'my-notes/dist'].every((rel) => !defaults.includes(rel)),
  { tree: defaults },
)
check(
  '1.ignore-is-not-skipped',
  (await ev(`document.querySelector('.skip-link') === null`)) === true,
  { skipEntry: await ev(`document.querySelector('.skip-link')?.textContent ?? null`) },
)

/* ═══════════════════════ 2. 入口常驻 + 面板可读 ═══════════════════════ */

const entry = await ignoreLink()
check('2.entry-is-always-there', entry !== null && entry.includes('忽略规则'), { entry })

const panel = await openPanel()
check('2.panel-opens', panel !== null, { panel: panel !== null })
check(
  '2.lists-fifteen-builtin-rules',
  (panel?.builtin.length ?? 0) === 15,
  { count: panel?.builtin.length ?? 0, names: (panel?.builtin ?? []).map((r) => r.name) },
)
check(
  '2.shows-config-path',
  (panel?.configPath ?? '').replace(/\\/g, '/').endsWith('.knowisle/config.toml'),
  { path: panel?.configPath ?? null },
)
check('2.no-error-when-config-is-absent', panel?.error === null, { error: panel?.error ?? null })
check(
  '2.no-user-rules-yet',
  (panel?.user.length ?? 0) === 0 && (panel?.empty ?? '').includes('还没有写过规则'),
  { user: panel?.user ?? [], empty: panel?.empty ?? null },
)
check(
  '2.says-rescan-is-required',
  (panel?.headings ?? []).some((text) => text.includes('改完要重新扫描')),
  { headings: panel?.headings ?? [] },
)
await closePanel()

/* ═══════════════════════ 3. extra + negate 生效 ═══════════════════════ */

writeConfig('[ignore]\nextra = ["my-notes"]\nnegate = ["logseq"]\n')
await reload()

const withRules = (await readTree()) ?? []
check('3.negate-brings-logseq-back', withRules.includes('logseq/pages'), { tree: withRules })
// 点开 `pages` 之后文件必须真的在：只看到目录行证明不了笔记回来了。
await expandRow('logseq/pages')
const expanded = (await readTree()) ?? []
check('3.negated-note-is-reachable', expanded.includes('logseq/pages/a.md'), { tree: expanded })
check('3.extra-hides-my-notes', !withRules.includes('my-notes/keep.md'), { tree: withRules })
check(
  '3.other-builtin-still-applied',
  !withRules.includes('dist') && !withRules.includes('node_modules'),
  { tree: withRules },
)

const rulePanel = await openPanel()
const logseqRule = (rulePanel?.builtin ?? []).find((rule) => rule.name === 'logseq')
check(
  '3.panel-marks-negated-builtin',
  logseqRule !== undefined && logseqRule.disabled && logseqRule.flags.includes('已停用'),
  { rule: logseqRule ?? null },
)
const extraRule = (rulePanel?.user ?? []).find((rule) => rule.name === 'my-notes')
check(
  '3.panel-lists-user-rule',
  extraRule !== undefined && extraRule.flags.includes('额外忽略'),
  { user: rulePanel?.user ?? [] },
)
check('3.no-error-for-a-valid-config', rulePanel?.error === null, { error: rulePanel?.error ?? null })
await closePanel()

/* ═══════════════════════ 4. 写坏：退回默认 + 可看见 ═══════════════════════ */

writeConfig('[ignore]\nextra = "dist"\n')
await reload()

const broken = (await readTree()) ?? []
check(
  '4.tree-falls-back-to-default',
  broken.includes('my-notes/keep.md') &&
    !broken.includes('logseq') &&
    !broken.includes('my-notes/dist'),
  { tree: broken },
)
const brokenEntry = await ignoreLink()
check(
  '4.entry-flags-the-error',
  (brokenEntry ?? '').includes('配置有误'),
  { entry: brokenEntry },
)

const brokenPanel = await openPanel()
check(
  '4.error-has-a-line-number',
  (brokenPanel?.error ?? '').includes('第 2 行'),
  { error: brokenPanel?.error ?? null },
)
check(
  '4.error-is-plain-language',
  (brokenPanel?.error ?? '').includes('必须是字符串数组'),
  { error: brokenPanel?.error ?? null },
)
check(
  '4.error-points-at-the-file',
  (brokenPanel?.errorPath ?? '').replace(/\\/g, '/').endsWith('.knowisle/config.toml'),
  { path: brokenPanel?.errorPath ?? null },
)
check(
  '4.error-says-nothing-applied',
  (brokenPanel?.errorNote ?? '').includes('一条都没生效'),
  { note: brokenPanel?.errorNote ?? null },
)
await closePanel()

/* ═══════════════════════ 5. .git 不可 negate ═══════════════════════ */

writeConfig('[ignore]\nnegate = [".git"]\n')
await reload()

const gitTree = (await readTree()) ?? []
check('5.git-stays-out-of-the-tree', !gitTree.includes('.git/config.md'), { tree: gitTree })

const gitPanel = await openPanel()
check(
  '5.refusal-names-git',
  (gitPanel?.error ?? '').includes('.git') && (gitPanel?.error ?? '').includes('永远被忽略'),
  { error: gitPanel?.error ?? null },
)
check(
  '5.refusal-is-not-silent',
  gitPanel?.error !== null,
  { error: gitPanel?.error ?? null },
)
await closePanel()

/* ═══════════════════════ 6. 删掉配置 → 回到默认 ═══════════════════════ */

clearConfig()
await reload()

const restored = (await readTree()) ?? []
check(
  '6.deleting-the-config-restores-defaults',
  restored.includes('my-notes/keep.md') &&
    !restored.includes('logseq') &&
    !restored.includes('my-notes/dist'),
  { tree: restored },
)

const restoredPanel = await openPanel()
check('6.no-error-after-deleting', restoredPanel?.error === null, {
  error: restoredPanel?.error ?? null,
})
await closePanel()

/* ═══════════════════════ 收尾 ═══════════════════════ */

check('X.no-console-errors', consoleErrors.length === 0, consoleErrors.slice(0, 5))

clearConfig()
killApp()
await sleep(400)

const failed = results.filter((r) => r.ok === false)
const skipped = results.filter((r) => r.ok === null)
console.log('')
console.log(
  `${failed.length === 0 ? '✅' : '❌'} 共 ${results.length} 项：${results.length - failed.length - skipped.length} 通过 / ${failed.length} 失败 / ${skipped.length} 跳过`,
)
fs.writeFileSync(
  path.join(OUT_DIR, `ignore-rules-acceptance-${Date.now()}.json`),
  JSON.stringify({ results, consoleErrors, platform: process.platform }, null, 2),
)
app.close()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
