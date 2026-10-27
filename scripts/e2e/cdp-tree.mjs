// M2 第四刀 + 第五刀 · 文件树的端到端验收。真实 WebView2 + CDP。
//
// 覆盖「新建 / 重命名 / 删除」三个动作，以及它们共用的那条路径闸：
//   新建：落点（根目录 / 当前笔记目录）、零字节、立即可输入、不覆盖、非法名 7 例
//   重命名：不覆盖、只改大小写、改当前打开的笔记（滚动/光标/撤销栈）、三处同步
//   删除：进回收站、sha 不变、删当前笔记、不重建编辑器、取消零副作用、同名两次
//   路径闸：UI 的非法名与 IPC 的 PATH_ESCAPE 是两道闸，各验一次
//   槽位：删掉再新建同一个路径，第一次保存不冲突（槽位键真的清空了）
//
// # 为什么两个脚本合成一个（对账结论）
//
// `cdp-create-rename.mjs`（52 项）与 `cdp-delete.mjs`（37 项）操作同一棵树、
// 同一份 1000 篇夹具。逐条比对后的结论是**没有重复判据**——看起来重叠的
// 四组其实各验一层：
//
//   | 看起来重叠 | 实际 |
//   |---|---|
//   | 新建不覆盖 / 重命名不覆盖 | 两个不同的命令，覆盖语义各自独立 |
//   | 删 `../x.md` → PATH_ESCAPE / 新建 `../x` → UI 拒绝 | IPC 闸 vs 前端校验 |
//   | `note_rename` 越界 / `note_create` 越界 | 两条命令共用闸，但各自要有证据 |
//   | 删后新建同路径 / 普通新建 | 前者验槽位键清空，后者验落点 |
//
// 真正重复的是**辅助函数**（`expandDir` / `openNodeMenu` / `sha` / 右键菜单
// 降级…）与**前置断言**（菜单开没开、浮层开没开）。辅助函数在这里只留一份；
// 前置断言各留一份，因为它们各自服务于不同的失败诊断——删掉任何一条，
// 失败时会变成「某一步莫名 false」，而不是「右键菜单没开」。
//
// 顺序是**先删后建**：删除段假设 `notes/note-000x.md` 与 `notes/foobar.md`
// 都在；新建段会把 `foo/bar.md` 改名、把 `notes/foobar.md` 改名，先跑会把
// 删除段的夹具弄丢。两段之间不重置夹具——重置要重扫，而重扫会把「删完立刻
// 看」这类断言的起点搅浑。
//
// 前置（缺一条都不算数）：
//   1. `pnpm dev`：vite 在 127.0.0.1:1420（脚本会取 /src/App.tsx 核对转译结果，B-18）
//   2. debug 产物存在：`cargo build -p knowisle`
//   3. 应用**刚重启**，带 `--remote-debugging-port=9222`（B-16：调试槽位是进程级静态）
//      或 `pnpm e2e:tree --restart` 让脚本自己杀掉并重启
//   4. 运行期间源码冻结
//
// 夹具：`os.tmpdir()/knowisle-e2e-vault-quick`（1000 篇 + 2 个坏文件），
// 每次全量重写（B-32）。应用处于「没有库」的状态时才建，避免自动保存把
// 刚删掉的文件写回来。
//
// 跑法：`pnpm e2e:tree`（或加 `--restart`）
// 退出码只有三种（B-19）：0 全跑全过 / 1 有失败 / 2 跑过但有跳过。
import fs from 'node:fs'
import path from 'node:path'
import crypto from 'node:crypto'

import { openApp, MOD } from './cdp.mjs'
import { OUT_DIR, QUICK_VAULT as VAULT, sleep } from './env.mjs'
import { prepareQuickVault } from './fixtures.mjs'

const TRASH = path.join(VAULT, '.knowisle', 'trash')
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

/* ───────────────────────── 磁盘侧工具 ───────────────────────── */

const sha = (rel) => {
  const file = path.join(VAULT, rel)
  if (!fs.existsSync(file)) return null
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex')
}
const shaOfFile = (abs) =>
  fs.existsSync(abs) ? crypto.createHash('sha256').update(fs.readFileSync(abs)).digest('hex') : null

/** 回收目录里所有条目（文件名 + 大小 + sha），按名字排序。 */
const trashList = () => {
  if (!fs.existsSync(TRASH)) return []
  return fs
    .readdirSync(TRASH)
    .sort()
    .map((name) => ({
      name,
      size: fs.statSync(path.join(TRASH, name)).size,
      sha: shaOfFile(path.join(TRASH, name)),
    }))
}
/** 回收目录里以 `-<原名>` 结尾的条目。 */
const trashFor = (rel) => {
  const base = rel.split('/').pop()
  return trashList().filter((f) => f.name.endsWith(`-${base}`))
}
const trashText = (name) => fs.readFileSync(path.join(TRASH, name), 'utf8')

/** 递归列出所有 .md（POSIX 相对路径），用来断言"磁盘上没有新文件"。 */
const walk = (dir, prefix) => {
  const out = []
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const rel = prefix === '' ? entry.name : `${prefix}/${entry.name}`
    if (entry.isDirectory()) out.push(...walk(path.join(dir, entry.name), rel))
    else if (/\.(md|markdown)$/i.test(entry.name)) out.push(rel)
  }
  return out
}
const listNotes = () => walk(VAULT, '').sort()

/* ───────────────────────── 会话 ───────────────────────── */

const consoleErrors = []
let app
let send
let ev
let waitFor
let press
let rawKey
let insertText
let clickAt

const bind = (session) => {
  app = session
  send = session.send
  ev = session.ev
  waitFor = session.waitFor
  press = session.press
  rawKey = session.rawKey
  insertText = session.insertText
  clickAt = session.clickAt
}

bind(await openApp({ vault: VAULT, prepare: prepareQuickVault, errors: consoleErrors }))

/* ───────────────────── 输入合成 ───────────────────── */

const ENTER = () => press('Enter', 'Enter', 13)
const ESC = () => press('Escape', 'Escape', 27)
const CTRL_S = () => press('s', 'KeyS', 83, MOD.ctrl)
const CTRL_Z = () => press('z', 'KeyZ', 90, MOD.ctrl)
const CTRL_P = () => press('p', 'KeyP', 80, MOD.ctrl)
const CTRL_END = () => press('End', 'End', 35, MOD.ctrl)
const CTRL_SHIFT_F = () => press('f', 'KeyF', 70, MOD.ctrl | MOD.shift)

/* ───────────────────────── 树 / 浮层工具 ───────────────────────── */

const treeTitles = () => ev(`[...document.querySelectorAll('.tree-row')].map((el) => el.title)`)

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

/** 点一个树节点（打开笔记），等编辑器里的 breadcrumb 变成它。 */
const openNote = async (title) => {
  const rect = await nodeRect(title)
  if (rect === null) return false
  await clickAt(rect.x, rect.y)
  const ok = await waitFor(
    `document.querySelector('.breadcrumb')?.textContent === ${JSON.stringify(title)}`,
    5000,
  )
  // 面包屑与编辑器在**同一次提交**里换，但 CM6 的 EditorView 是在 passive
  // effect 里创建的——它可能晚于面包屑一帧。不 settle 就去点 `.cm-content`，
  // 点到的是即将销毁的旧视图。
  if (ok) await sleep(150)
  return ok
}

/**
 * 右键一个树节点 → 点菜单里的某一项 → 等目标浮层出现。
 *
 * 真实右键走 CDP 鼠标事件；WebView2 若没把它转成 `contextmenu`，退化成合成
 * 事件——React 的事件委托照样能收到，走的仍是应用自己的处理函数，不是桩。
 */
const openNodeMenu = async (title) => {
  const rect = await nodeRect(title)
  if (rect === null) return { ok: false, reason: `树里没有 ${title}` }
  await send('Input.dispatchMouseEvent', { type: 'mousePressed', x: rect.x, y: rect.y, button: 'right', buttons: 2, clickCount: 1 })
  await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: rect.x, y: rect.y, button: 'right', buttons: 0, clickCount: 1 })
  let opened = await waitFor(`document.querySelector('.context-menu') !== null`, 800)
  let via = 'cdp-mouse'
  if (!opened) {
    via = 'synthetic'
    await ev(`(() => {
      const el = [...document.querySelectorAll('.tree-row')].find((e) => e.title === ${JSON.stringify(title)})
      const r = el.getBoundingClientRect()
      el.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.left + 30, clientY: r.top + r.height / 2 }))
      return true
    })()`)
    opened = await waitFor(`document.querySelector('.context-menu') !== null`, 800)
  }
  return { ok: opened, via }
}

const menuLabels = () => ev(`[...document.querySelectorAll('.context-menu__item')].map((e) => e.textContent.trim())`)

const clickMenuItem = async (label, expectSelector = '.prompt') => {
  const rect = await ev(`(() => {
    const el = [...document.querySelectorAll('.context-menu__item')].find((e) => e.textContent.trim() === ${JSON.stringify(label)})
    if (!el) return null
    const r = el.getBoundingClientRect()
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
  })()`)
  if (rect === null) return false
  await clickAt(rect.x, rect.y)
  return waitFor(`document.querySelector(${JSON.stringify(expectSelector)}) !== null`, 1500)
}

/* ───────────────────────── 名字浮层 ───────────────────────── */

const promptState = () =>
  ev(`(() => {
    const root = document.querySelector('.prompt')
    if (root === null) return { open: false }
    const input = root.querySelector('.prompt__input')
    const errors = [...root.querySelectorAll('.prompt__error')].map((el) => el.textContent ?? '')
    const confirm = root.querySelector('.btn--primary')
    return {
      open: true,
      title: root.querySelector('.prompt__title')?.textContent ?? '',
      value: input?.value ?? null,
      errors,
      confirmDisabled: confirm?.disabled === true,
      confirmLabel: confirm?.textContent ?? '',
      focus: document.activeElement?.className ?? '',
    }
  })()`)

const openCreatePrompt = async () => {
  const rect = await ev(`(() => {
    const el = document.querySelector('.sidebar__head .btn')
    if (el === null) return null
    const r = el.getBoundingClientRect()
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
  })()`)
  if (rect === null) return false
  await clickAt(rect.x, rect.y)
  return waitFor(`document.querySelector('.prompt') !== null`, 1500)
}

/**
 * 名字浮层里输入并提交，返回提交后的浮层状态（还开着 = 被拒绝了）。
 *
 * 不能用一个固定 sleep 等结果：`note_rename` 会 drain 自动保存的 worker，
 * 而 worker 在节流窗口里是**睡着**的（最多 1.5 秒才醒）。刚落盘过的笔记
 * 立刻改名，这条链路就会等它睡醒——固定 420ms 会把"慢但正确"误判成失败。
 */
const submitName = async (name, { clearFirst = false } = {}) => {
  if (clearFirst) {
    await ev(`document.querySelector('.prompt__input').select()`)
    await rawKey('Delete', 'Delete', 46)
  }
  await insertText(name)
  await sleep(60)
  const t0 = Date.now()
  await ENTER()
  await waitFor(
    `(() => {
      const p = document.querySelector('.prompt')
      if (p === null) return true
      return (p.querySelector('.prompt__error')?.textContent ?? '') !== ''
    })()`,
    6000,
  )
  return { ...(await promptState()), ms: Date.now() - t0 }
}

const closePromptIfOpen = async () => {
  if (!(await promptState()).open) return
  await ESC()
  await waitFor(`document.querySelector('.prompt') === null`, 1500)
}

/* ───────────────────────── 删除确认框 ───────────────────────── */

const confirmState = () =>
  ev(`(() => {
    const d = document.querySelector('.confirm')
    if (d === null) return { open: false }
    const danger = d.querySelector('.btn--danger')
    const cancel = [...d.querySelectorAll('.btn')].find((b) => b.textContent.trim() === '取消')
    // 期望色**在页面里**解析 token，不写死 rgb：暗色主题下 --ki-danger-solid
    // 指向 red-700（#b91c1c），亮色下指向 red-600（#dc2626）。写死其中一个，
    // 这个脚本就只能在「上一次跑完留下的主题」恰好是那一个时才绿。
    const probe = document.createElement('div')
    probe.style.background = 'var(--ki-danger-solid)'
    document.body.appendChild(probe)
    const dangerToken = getComputedStyle(probe).backgroundColor
    probe.remove()
    return {
      open: true,
      title: d.querySelector('.confirm__title')?.textContent ?? '',
      body: d.querySelector('.confirm__body')?.textContent ?? '',
      note: d.querySelector('.confirm__note')?.textContent ?? '',
      dangerLabel: danger?.textContent ?? '',
      dangerBg: danger === null ? null : getComputedStyle(danger).backgroundColor,
      dangerToken,
      theme: document.documentElement.dataset.theme ?? null,
      focusIsCancel: document.activeElement === cancel,
      focusText: document.activeElement?.textContent ?? '',
    }
  })()`)

/**
 * 点确认框里的「删除」，并在**页面内用 rAF** 量到「树里那一行消失 + 浮层关闭」。
 *
 * 用 rAF 而不是 `Date.now()`：删除是异步的（IPC + 重扫 + 重渲染），
 * 只有"哪一帧上它不见了"才是用户真正感知到的时刻。
 */
const confirmAndMeasure = (title) =>
  ev(`(async () => {
    const dialog = document.querySelector('.confirm')
    if (dialog === null) return { ok: false, reason: '没有确认框' }
    const btn = dialog.querySelector('.btn--danger')
    if (btn === null) return { ok: false, reason: '没有确认按钮' }
    const gone = () => ![...document.querySelectorAll('.tree-row--file')].some((el) => el.title === ${JSON.stringify(title)})
    const t0 = performance.now()
    btn.click()
    return await new Promise((resolve) => {
      const started = performance.now()
      const tick = () => {
        if (gone() && document.querySelector('.confirm') === null) {
          resolve({ ok: true, ms: performance.now() - t0 })
        } else if (performance.now() - started > 8000) {
          resolve({ ok: false, reason: '8 秒内树里那一行没消失' })
        } else {
          requestAnimationFrame(tick)
        }
      }
      requestAnimationFrame(tick)
    })
  })()`)

/**
 * 一步完成「右键 → 删除 → 确认」，全在一个 evaluate 里。
 *
 * 验收 2 要求"打字之后 300ms 内右键删除"——分成四次 CDP 往返（每次带
 * 100–150ms 的等待）必然超过 300ms，测到的就不是合并窗口那条路径了。
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

/** 等确认框关闭（删除成功）——`fastDelete` 只负责点，不负责等。 */
const waitConfirmClosed = () => waitFor(`document.querySelector('.confirm') === null`, 8000)
/** 确认框里的错误文案。删除失败时它是唯一线索。 */
const confirmError = () => ev(`document.querySelector('.confirm__error')?.textContent ?? null`)

/**
 * 用侧栏「＋ 新建」建一篇，返回它**实际落盘**的路径。
 *
 * 落点由应用决定（当前笔记所在目录，没选中就是根目录），所以路径只能读回来，
 * 不能假定。这个 helper 就是"照应用的规则读回事实"。
 */
const createHere = async (name) => {
  const dir = await ev(`(() => {
    const crumb = document.querySelector('.breadcrumb')?.textContent ?? null
    if (crumb === null) return ''
    const i = crumb.lastIndexOf('/')
    return i < 0 ? '' : crumb.slice(0, i)
  })()`)
  const rect = await ev(`(() => {
    const el = document.querySelector('.sidebar__head .btn')
    if (el === null) return null
    const r = el.getBoundingClientRect()
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
  })()`)
  if (rect === null) return { ok: false, path: null }
  await clickAt(rect.x, rect.y)
  if (!(await waitFor(`document.querySelector('.prompt__input') !== null`, 2000))) {
    return { ok: false, path: null, reason: '没打开新建浮层' }
  }
  await insertText(name)
  await ENTER()
  const closed = await waitFor(`document.querySelector('.prompt') === null`, 8000)
  const error = await ev(`document.querySelector('.prompt__error')?.textContent ?? null`)
  return { ok: closed, path: dir === '' ? `${name}.md` : `${dir}/${name}.md`, error }
}

/* ───────────────────────── 编辑器 / IPC ───────────────────────── */

/** 编辑器状态快照：DOM 身份 + 滚动 + 光标 + 可见内容 sha。 */
const editorSnapshot = () =>
  ev(`(async () => {
    const editor = document.querySelector('.cm-editor')
    if (editor === null) return { open: false }
    editor.dataset.kiProbe = 'probe-${STAMP}'
    const content = editor.querySelector('.cm-content')
    const scroller = editor.querySelector('.cm-scroller')
    const sel = document.getSelection()
    window.__kiSelNode = sel?.anchorNode ?? null
    const bytes = new TextEncoder().encode(content?.textContent ?? '')
    const digest = await crypto.subtle.digest('SHA-256', bytes)
    const hex = [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('')
    return {
      open: true,
      probe: 'probe-${STAMP}',
      scrollTop: scroller?.scrollTop ?? -1,
      scrollHeight: scroller?.scrollHeight ?? -1,
      cursorText: sel?.anchorNode?.textContent?.slice(0, 40) ?? null,
      cursorOffset: sel?.anchorOffset ?? -1,
      contentSha: hex,
      contentLen: bytes.length,
    }
  })()`)

const editorSnapshotAfter = () =>
  ev(`(async () => {
    const editor = document.querySelector('.cm-editor')
    if (editor === null) return { open: false }
    const content = editor.querySelector('.cm-content')
    const scroller = editor.querySelector('.cm-scroller')
    const sel = document.getSelection()
    const bytes = new TextEncoder().encode(content?.textContent ?? '')
    const digest = await crypto.subtle.digest('SHA-256', bytes)
    return {
      open: true,
      probe: editor.dataset.kiProbe ?? null,
      sameEditorNode: editor.dataset.kiProbe === 'probe-${STAMP}',
      scrollTop: scroller?.scrollTop ?? -1,
      cursorText: sel?.anchorNode?.textContent?.slice(0, 40) ?? null,
      cursorOffset: sel?.anchorOffset ?? -1,
      sameCursorNode: window.__kiSelNode !== null && sel?.anchorNode === window.__kiSelNode,
      contentSha: [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join(''),
      contentLen: bytes.length,
    }
  })()`)

/** 直接打 IPC。错误是序列化后的 KiError 对象（Throw 模式）。 */
const ipc = (cmd, args) =>
  ev(`(async () => {
    try {
      const value = await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args)})
      return { ok: true, value }
    } catch (err) {
      return { ok: false, error: typeof err === 'object' && err !== null ? err : { raw: String(err) } }
    }
  })()`)

const viewerMeta = () => ev(`document.querySelector('.viewer__meta')?.textContent ?? null`)
const conflictBanner = () => ev(`document.querySelector('.banner--warn')?.textContent ?? null`)
const statsText = () => ev(`document.querySelector('.stats')?.textContent ?? null`)

/* ───────────────────────── 准备 ───────────────────────── */

if (!(await expandDir('notes'))) throw new Error('展不开 notes 目录')
const rootShown = await ev(`document.querySelector('.sidebar__root')?.title ?? null`)
log(`vault = ${rootShown}`)
log(`stats = ${await statsText()}`)
consoleErrors.length = 0 // 重载之前的错误不算

/* ═══════════════════════════════════════════════════════════════════════
 * 删除段（原 cdp-delete.mjs 验收 1–12）
 *
 * 排在新建段前面：这一段假设 `notes/note-000x.md` 与 `notes/foobar.md` 都还
 * 在原名上，而新建段会把 `foo/bar.md` 改名成 `Bar.md`、把 `notes/foobar.md`
 * 改名成 `notes/foobar-改名-<stamp>.md`。反过来跑，删除段的夹具就没了。
 * ═══════════════════════════════════════════════════════════════════════ */

/* ───────────── 验收 1：删除非当前笔记（树里消失的耗时 + trash + sha） ───────────── */

{
  const opened = await openNote('notes/note-0000.md')
  const before = sha('notes/note-0004.md')
  const menu = await openNodeMenu('notes/note-0004.md')
  const labels = menu.ok ? await menuLabels() : []
  const dialog = menu.ok ? await clickMenuItem('删除', '.confirm') : false
  const state = await confirmState()

  check('1a 右键菜单含「删除」', menu.ok && labels.includes('删除'), { via: menu.via, labels })
  // 文案在 M5 第三刀改过：面板里能恢复了，所以「没有恢复入口」换成了
  // 「可以恢复回原目录」。判据跟着改，但仍然卡住三件事——落到哪个目录、
  // 不是永久删除、以及**怎么才真的删掉**（用户最需要知道的那一句）。
  check(
    '1b 确认框文案说清 trash / 非永久删除 / 怎么才真删',
    state.open &&
      state.body.includes('.knowisle/trash/') &&
      state.note.includes('不是永久删除') &&
      state.note.includes('可以恢复回原目录') &&
      state.note.includes('清空回收站才会真正删掉'),
    { body: state.body, note: state.note },
  )
  check('1c 默认焦点在「取消」', state.open && state.focusIsCancel, { focusText: state.focusText })
  check('1d 危险按钮用 --ki-danger-solid', state.dangerBg === state.dangerToken, {
    bg: state.dangerBg,
    token: state.dangerToken,
    theme: state.theme,
  })

  const measured = dialog ? await confirmAndMeasure('notes/note-0004.md') : { ok: false }
  const trashed = trashFor('notes/note-0004.md')
  check('1e 删除成功且树里消失', opened && measured.ok === true, measured)
  check(
    '1f trash 里出现且 sha 与删除前一致',
    trashed.length === 1 && before !== null && trashed[0].sha === before,
    { before, trashed: trashed.map((f) => f.name) },
  )
  check('1g 原路径已不存在', !fs.existsSync(path.join(VAULT, 'notes/note-0004.md')), null)
}

/* ───────────── 验收 3：删除后 3 秒不复活（跨 1.5s 节拍 + 300ms 合并窗口） ───────────── */

{
  const started = Date.now()
  await sleep(3200)
  const alive = fs.existsSync(path.join(VAULT, 'notes/note-0004.md'))
  const extra = trashFor('notes/note-0004.md').length
  check('3 删除后 3.2 秒文件没有复活', !alive && extra === 1, {
    waitedMs: Date.now() - started,
    alive,
    trashCopies: extra,
  })
}

/* ───────────── 验收 2：有未落盘内容时删除（trash 里必须含标记） ───────────── */

{
  const marker = `未落盘标记-${STAMP}`
  const opened = await openNote('notes/note-0002.md')
  await ev(`document.querySelector('.cm-content').focus(); true`)
  await CTRL_END()
  const typedAt = Date.now()
  await insertText(marker)
  const fast = await fastDelete('notes/note-0002.md')
  const elapsed = Date.now() - typedAt
  const closed = await waitConfirmClosed()
  const failure = await confirmError()
  const trashed = trashFor('notes/note-0002.md')
  const text = trashed.length === 1 ? trashText(trashed[0].name) : ''
  check('2a 打字到点确认 ≤ 300ms（合并窗口内）', fast.ok && elapsed <= 300, {
    elapsedMs: elapsed,
    fast,
  })
  check('2b trash 里的内容含那个标记（drain 生效）', closed && text.includes(marker), {
    opened,
    closed,
    failure,
    trash: trashed.map((f) => f.name),
    containsMarker: text.includes(marker),
    tail: text.slice(-60),
  })
}

/* ───────────── 验收 4：删除当前打开的笔记 ───────────── */

{
  const opened = await openNote('notes/note-0006.md')
  const errorsBefore = consoleErrors.length
  const hadEditor = await ev(`document.querySelector('.cm-editor') !== null`)
  const menu = await openNodeMenu('notes/note-0006.md')
  await clickMenuItem('删除', '.confirm')
  const measured = await confirmAndMeasure('notes/note-0006.md')
  await sleep(300)

  const editorGone = await ev(`document.querySelector('.cm-editor') === null`)
  const welcome = await ev(`[...document.querySelectorAll('.welcome p')].map((p) => p.textContent)`)
  const welcomeAll = (welcome ?? []).join('|')
  const errorsAfter = consoleErrors.length
  const status = await ipc('note_status', { root: rootShown, path: 'notes/note-0006.md' })

  check('4a 删除当前笔记成功且编辑器卸载', opened && hadEditor && measured.ok === true && editorGone, {
    opened,
    hadEditor,
    measured,
    editorGone,
  })
  check(
    '4b 空状态文案是「还没选中笔记」而不是「写第一篇」',
    welcomeAll.includes('还没选中笔记') && !welcomeAll.includes('写第一篇'),
    { welcome },
  )
  check('4c 控制台无新报错', errorsAfter === errorsBefore, {
    before: errorsBefore,
    after: errorsAfter,
    last: consoleErrors.slice(-3),
  })
  check('4d note_status 不抛（删除后查已删路径）', status.ok === true, status)
}

/* ───────────── 验收 5：删除非当前笔记，编辑器不重建 ───────────── */

{
  const opened = await openNote('notes/foobar.md')
  // 滚到中段并把光标放进某一行：重建会同时丢掉滚动位置与光标。
  await ev(`(() => {
    const scroller = document.querySelector('.cm-scroller')
    scroller.scrollTop = 4000
    return true
  })()`)
  await sleep(200)
  const lineRect = await ev(`(() => {
    const lines = [...document.querySelectorAll('.cm-line')]
    const target = lines.find((el) => (el.textContent ?? '').includes('长文填充行 200'))
    if (!target) return null
    const r = target.getBoundingClientRect()
    return { x: r.left + 12, y: r.top + r.height / 2, text: target.textContent }
  })()`)
  if (lineRect !== null) await clickAt(lineRect.x, lineRect.y)
  await sleep(150)

  const before = await editorSnapshot()
  const menu = await openNodeMenu('notes/note-0008.md')
  await clickMenuItem('删除', '.confirm')
  const measured = await confirmAndMeasure('notes/note-0008.md')
  await sleep(400)
  const after = await editorSnapshotAfter()

  check(
    '5a 删别的笔记成功，当前笔记仍在编辑',
    opened && measured.ok === true && before.open && after.open,
    { opened, measured },
  )
  check(
    '5b scrollTop 一字不变',
    before.open && after.open && before.scrollTop === after.scrollTop && before.scrollTop > 0,
    { before: before.scrollTop, after: after.scrollTop },
  )
  check(
    '5c 光标没动（同一文本节点 + 同一偏移）',
    after.sameCursorNode && before.cursorOffset === after.cursorOffset,
    { before: [before.cursorText, before.cursorOffset], after: [after.cursorText, after.cursorOffset] },
  )
  check(
    '5d 可见内容 sha 一字不变',
    before.open && after.open && before.contentSha === after.contentSha && before.contentLen === after.contentLen,
    { before: before.contentSha, after: after.contentSha, len: [before.contentLen, after.contentLen] },
  )
  check('5e 编辑器 DOM 没重建', after.sameEditorNode === true, { probe: after.probe })
}

/* ───────────── 验收 6：取消删除，零副作用 ───────────── */

{
  const before = sha('notes/note-0010.md')
  const trashBefore = trashList().length
  const treeBefore = (await treeTitles()).join('|')
  const menu = await openNodeMenu('notes/note-0010.md')
  await clickMenuItem('删除', '.confirm')
  const state = await confirmState()
  await ESC()
  const closed = await waitFor(`document.querySelector('.confirm') === null`, 1500)
  await sleep(200)

  const after = sha('notes/note-0010.md')
  const treeAfter = (await treeTitles()).join('|')
  check('6a Esc 关掉确认框', menu.ok && state.open && closed, { state: state.open, closed })
  check('6b 文件 sha 不变、树不变、trash 没多东西', before === after && trashBefore === trashList().length && treeBefore === treeAfter, {
    sha: [before, after],
    trash: [trashBefore, trashList().length],
    treeSame: treeBefore === treeAfter,
  })
}

/* ───────────── 验收 7：同名两次删除，两个文件都在 ───────────── */

{
  /** 打字 → Ctrl+S 落盘。返回是否成功。 */
  const typeInto = async (text, rel) => {
    if (!(await waitFor(`document.querySelector('.cm-content') !== null`, 5000))) return false
    await ev(`document.querySelector('.cm-content').focus(); true`)
    await CTRL_END()
    await insertText(text)
    await CTRL_S()
    // 等**磁盘**上出现内容，而不是等某个通道的回执：Ctrl+S 与 1.5s 自动落盘
    // 都是合法路径，这里只关心"删之前它已经落盘了"。
    for (let i = 0; i < 60; i += 1) {
      if (fs.existsSync(path.join(VAULT, rel)) && fs.statSync(path.join(VAULT, rel)).size > 0) break
      await sleep(100)
    }
    return true
  }

  // 两次都先在 notes/foobar.md 里（同一个目录），这样「新建」落在同一个
  // 路径上——两次删除才是同一个 a.md 的两次删除，而不是两个不同文件。
  await openNote('notes/foobar.md')
  const first = await createHere('dup')
  await typeInto('第一版内容', first.path)
  const firstSha = sha(first.path)
  await fastDelete(first.path)
  const firstClosed = await waitConfirmClosed()
  const firstError = await confirmError()
  await sleep(200)

  await openNote('notes/foobar.md')
  const second = await createHere('dup')
  await typeInto('第二版内容', second.path)
  await fastDelete(second.path)
  const secondClosed = await waitConfirmClosed()
  const secondError = await confirmError()
  await sleep(200)

  const copies = trashFor('dup.md')
  const texts = copies.map((f) => trashText(f.name))
  check('7a trash 里有两份，名字不同', firstClosed && secondClosed && copies.length === 2 && copies[0].name !== copies[1].name, {
    first: { path: first.path, error: firstError },
    second: { path: second.path, error: secondError },
    names: copies.map((f) => f.name),
  })
  check('7b 两份内容各自对得上，没有互相覆盖', texts.includes('第一版内容') && texts.includes('第二版内容'), {
    texts,
    firstSha,
  })
  check('7c 原路径已不存在', !fs.existsSync(path.join(VAULT, second.path)), null)
}

/* ───────────── 验收 8 / 9 / 10：直接打 IPC 的三条拒绝 ───────────── */

// 判据是**错误码**，不是文案：M3 把「语义拒绝」从 `Io` 里拆了出来
// （`NotAFile` / `ReservedPath`），前端靠 code 决定给用户什么下一步。
// 这里跟着改成新码——断言旧码 `IO` 等于在验一条已经不存在的契约。
{
  const dir = await ipc('note_delete', { root: rootShown, path: 'notes/sub-a' })
  check(
    '8 传目录路径 → NOT_A_FILE，不是 panic',
    dir.ok === false && dir.error.code === 'NOT_A_FILE' && dir.error.detail?.path === 'notes/sub-a',
    dir,
  )
  check('8b 目录与里面的笔记都还在', fs.existsSync(path.join(VAULT, 'notes/sub-a')) && fs.readdirSync(path.join(VAULT, 'notes/sub-a')).length > 0, null)

  const trashNames = trashList().map((f) => f.name)
  const victim = trashNames[0] ?? null
  const inner = victim === null ? { ok: true, skipped: true } : await ipc('note_delete', { root: rootShown, path: `.knowisle/trash/${victim}` })
  if (victim === null) skip('9 传 .knowisle/trash/x.md → 拒绝', '回收目录还是空的')
  else
    check(
      '9 传 .knowisle/trash/x.md → RESERVED_PATH，且那份文件还在',
      inner.ok === false && inner.error.code === 'RESERVED_PATH' && fs.existsSync(path.join(TRASH, victim)),
      { victim, inner },
    )

  const escape = await ipc('note_delete', { root: rootShown, path: '../x.md' })
  check('10 传 ../x.md → PATH_ESCAPE', escape.ok === false && escape.error.code === 'PATH_ESCAPE', escape)

  const meta = await ipc('note_rename', { root: rootShown, path: `.knowisle/trash/${victim ?? 'x.md'}`, name: 'y' })
  check('10b note_rename 也被同一条闸挡住（RESERVED_PATH）', meta.ok === false && meta.error.code === 'RESERVED_PATH', meta)
}

/* ───────────── 验收 11：槽位真的清空（删 → 新建 → 打字 → 保存不冲突） ───────────── */

{
  // 上一步删掉的是当前笔记，`activeNote` 已经是 null → 新建落在**根目录**，
  // 两次都是 `slot.md`，也就是同一个 (root, path)——槽位键正是这个。
  const first = await createHere('slot')
  if (!(await waitFor(`document.querySelector('.cm-content') !== null`, 5000))) {
    throw new Error('新建之后编辑器没挂载')
  }
  await ev(`document.querySelector('.cm-content').focus(); true`)
  await CTRL_END()
  await insertText('槽位第一版')
  await CTRL_S()
  await sleep(400)
  const firstSha = sha(first.path)
  await fastDelete(first.path)
  const firstClosed = await waitConfirmClosed()
  await sleep(200)

  const second = await createHere('slot')
  if (!(await waitFor(`document.querySelector('.cm-content') !== null`, 5000))) {
    throw new Error('第二次新建之后编辑器没挂载')
  }
  await ev(`document.querySelector('.cm-content').focus(); true`)
  await CTRL_END()
  await insertText('槽位第二版')
  await sleep(60)
  await CTRL_S()
  await sleep(700)

  const meta = await viewerMeta()
  const banner = await conflictBanner()
  const status = await ipc('note_status', { root: rootShown, path: second.path })
  const disk = sha(second.path) === null ? null : fs.readFileSync(path.join(VAULT, second.path), 'utf8')
  check('11a 新建落在同一个路径上（同一个槽位键）', first.path === second.path && first.path === 'slot.md', {
    first: first.path,
    second: second.path,
    firstSha,
    firstClosed,
  })
  check('11b 重新打开同名笔记后第一次保存不冲突', banner === null, { banner, meta })
  check('11c 保存状态干净（无 lastError）', status.ok === true && status.value?.lastError === null, status)
  check('11d 第二版真的落盘了', disk !== null && disk.includes('槽位第二版'), { disk })
}

/* ───────────── 验收 12：搜索（删掉的不再命中 + 建索引 1 次 + 重扫耗时） ───────────── */

{
  const marker = `索引探针-${STAMP}`
  await openNote('notes/note-0012.md')
  await ev(`document.querySelector('.cm-content').focus(); true`)
  await CTRL_END()
  await insertText(marker)
  await CTRL_S()
  await sleep(500)

  const search = async (needle) => {
    await CTRL_SHIFT_F()
    if (!(await waitFor(`document.querySelector('.search__input') !== null`, 2000))) return null
    await ev(`document.querySelector('.search__input').focus(); true`)
    await insertText(needle)
    // 等「结果那一版」：footer 里出现「建索引」（B-22）
    await waitFor(`(document.querySelector('.search__footer')?.textContent ?? '').includes('建索引')`, 15000)
    const snapshot = await ev(`(() => ({
      footer: document.querySelector('.search__footer')?.textContent ?? '',
      items: [...document.querySelectorAll('.search__path')].map((e) => e.textContent),
      empty: document.querySelector('.search__empty')?.textContent ?? null,
    }))()`)
    await ESC()
    await waitFor(`document.querySelector('.search__input') === null`, 1500)
    return snapshot
  }

  const hit = await search(marker)
  check('12a 删除前搜得到（基线，footer 显示建索引 1 次）', hit !== null && hit.items.some((p) => p.includes('notes/note-0012.md')), hit)

  await fastDelete('notes/note-0012.md')
  await waitConfirmClosed()
  await sleep(300)

  // 0 命中时 footer 只显示「没有匹配的行」（`summaryOf` 的 early return），
  // 读不到 builds / indexMs，所以诊断数字走 IPC——它和前端调的是同一条命令。
  const after = await ipc('vault_search', { root: rootShown, query: marker, refreshPath: null })
  check(
    '12b 删掉之后不再命中，且重扫后恰好重建 1 次索引',
    after.ok === true && after.value.total === 0 && after.value.builds === 2 && after.value.indexMs > 0,
    { total: after.value?.total, builds: after.value?.builds, indexMs: after.value?.indexMs },
  )

  const miss = await search(marker)
  check('12c 界面上也搜不到（0 命中）', miss !== null && miss.items.length === 0, miss)

  // 重扫耗时：直接打 vault_scan（与前端删完调的是同一条命令）。
  const scan = await ipc('vault_scan', { root: rootShown })
  check('12d 整库重扫的耗时', scan.ok === true, {
    totalFiles: scan.value?.totalFiles,
    totalDirs: scan.value?.totalDirs,
    elapsedMs: scan.value?.elapsedMs,
  })
  console.log(`stats = ${await statsText()}`)
}

/* ═══════════════════════════════════════════════════════════════════════
 * 新建 / 重命名段（原 cdp-create-rename.mjs 验收 1–9）
 *
 * 与删除段共用同一棵树，不重置夹具：重置要重扫，而重扫会把「删完立刻看」
 * 那几条断言的起点搅浑（删除段 1e / 3 / 5 量的是同一次重扫的窗口）。
 * ═══════════════════════════════════════════════════════════════════════ */

/* ═══════════════════ 验收 1 · 新建（根目录 / 当前笔记目录） ═══════════════════ */

const rootName = `M2-根目录-${STAMP}`

check('1a.prompt-opens', await openCreatePrompt(), await promptState())
check(
  '1b.prompt-targets-root',
  (await promptState()).title === '新建笔记',
  (await promptState()).title,
)

// 先挂上等待，再敲 Enter：t0 在页面里取，少一次 CDP 往返的误差。
const rootWait = ev(`new Promise((resolve) => {
  const want = ${JSON.stringify(`${rootName}.md`)}
  window.__KI_T0 = performance.now()
  const tick = () => {
    const hit = [...document.querySelectorAll('.tree-row')].find((el) => el.title === want)
    if (hit) resolve(performance.now() - window.__KI_T0)
    else requestAnimationFrame(tick)
  }
  tick()
})`)
await insertText(rootName)
await ENTER()
const createMs = await rootWait
await waitFor(`document.querySelector('.prompt') === null`, 4000)

const rootRel = `${rootName}.md`
const rootAbs = path.join(VAULT, rootRel)
check('1c.file-exists', fs.existsSync(rootAbs), rootRel)
check('1d.zero-bytes', fs.existsSync(rootAbs) && fs.statSync(rootAbs).size === 0, {
  size: fs.existsSync(rootAbs) ? fs.statSync(rootAbs).size : null,
})
check('1e.in-tree', (await treeTitles()).includes(rootRel), 'tree')
check(
  '1f.opened',
  await waitFor(`document.querySelector('.breadcrumb')?.textContent === ${JSON.stringify(rootRel)}`, 3000),
  await ev(`document.querySelector('.breadcrumb')?.textContent ?? ''`),
)
check(
  '1g.editor-focused',
  await waitFor(`document.activeElement?.classList.contains('cm-content') === true`, 2000),
  await ev(`document.activeElement?.className ?? ''`),
)

// 可立即输入：插一行字，等自动保存，磁盘上必须出现。
const typedNow = `KI-立即输入-${STAMP}`
await insertText(typedNow)
check(
  '1h.typing-lands',
  await waitFor(`document.querySelector('.cm-content')?.innerText.includes(${JSON.stringify(typedNow)}) === true`, 2000),
  'editor DOM',
)
await sleep(2200)
check(
  '1i.typing-saved',
  fs.readFileSync(rootAbs, 'utf8') === typedNow,
  { disk: fs.readFileSync(rootAbs, 'utf8') },
)

// 验收 10a：从命令发出到树更新完成。上面已经量到（createMs）。
const statsAfterCreate = await ev(`document.querySelector('.stats')?.textContent ?? ''`)
check('10a.create-to-tree', createMs !== null && createMs < 1000, {
  ms: createMs === null ? null : Math.round(createMs),
  rescan: statsAfterCreate,
})

/* ── 1j. 当前笔记目录：先开 notes/foobar.md，再从侧栏新建 ── */

await CTRL_P()
await waitFor(`document.querySelector('.quick-open') !== null`, 1500)
await insertText('foobar')
await sleep(120)
await ENTER()
await waitFor(`document.querySelector('.breadcrumb')?.textContent === 'notes/foobar.md'`, 4000)

const subName = `M2-子目录-${STAMP}`
check('1j.prompt-opens', await openCreatePrompt(), await promptState())
const subTitle = await ev(`document.querySelector('.sidebar__head .btn')?.title ?? ''`)
check('1k.targets-active-dir', subTitle.includes('notes'), subTitle)
await submitName(subName)
await waitFor(`document.querySelector('.prompt') === null`, 4000)

const subRel = `notes/${subName}.md`
check('1l.created-in-active-dir', fs.existsSync(path.join(VAULT, subRel)), subRel)
check(
  '1m.revealed-in-tree',
  await waitFor(`[...document.querySelectorAll('.tree-row')].some((el) => el.title === ${JSON.stringify(subRel)})`, 2500),
  'notes/ 默认是折叠的（100 个子项），能看见就说明 reveal 生效',
)
check('1n.opened', await waitFor(`document.querySelector('.breadcrumb')?.textContent === ${JSON.stringify(subRel)}`, 3000), 'breadcrumb')

/* ═══════════════════ 验收 2 · 新建不覆盖 ═══════════════════ */

const conflictRel = 'docs/readme.md'
const before2 = sha(conflictRel)

const menu2 = await openNodeMenu('docs')
check('2a.context-menu', menu2.ok, menu2)
check('2b.menu-new', await clickMenuItem('新建笔记'), await promptState())
const rejected2 = await submitName('readme')
check('2c.rejected', rejected2.open && rejected2.errors.length > 0, rejected2)
check('2d.sha-unchanged', sha(conflictRel) === before2, { before: before2, after: sha(conflictRel) })
await closePromptIfOpen()

/* ═══════════════════ 验收 3 · 重命名不覆盖 ═══════════════════ */

const aRel = 'foo/bar.md'
const bRel = 'foo/foo-0000.md'
const beforeA = sha(aRel)
const beforeB = sha(bRel)

check('3a0.expand-foo', await expandDir('foo'), 'foo/ 有 40 个子项，默认折叠')
const menu3 = await openNodeMenu(aRel)
check('3a.context-menu', menu3.ok, menu3)
check('3b.menu-rename', await clickMenuItem('重命名'), await promptState())
const rejected3 = await submitName('foo-0000', { clearFirst: true })
check('3c.rejected', rejected3.open && rejected3.errors.length > 0, rejected3)
check(
  '3d.both-sha-unchanged',
  sha(aRel) === beforeA && sha(bRel) === beforeB,
  { a: [beforeA, sha(aRel)], b: [beforeB, sha(bRel)] },
)
await closePromptIfOpen()

/* ═══════════════════ 验收 4 · 只改大小写 ═══════════════════ */

const menu4 = await openNodeMenu(aRel)
check('4a.context-menu', menu4.ok, menu4)
await clickMenuItem('重命名')
// 输入框里预填的是 `bar.md`，主名 `bar` 已被选中，直接打 `Bar` 即可。
const accepted4 = await submitName('Bar')
await waitFor(`document.querySelector('.prompt') === null`, 4000)

const fooEntries = fs.readdirSync(path.join(VAULT, 'foo')).filter((n) => /^bar\.md$/i.test(n))
check('4b.accepted', accepted4.open === false, accepted4)
check('4c.one-entry-new-casing', fooEntries.length === 1 && fooEntries[0] === 'Bar.md', fooEntries)

// 「旧名字没了」不能用 `existsSync`：Windows 的路径解析**不区分大小写**，
// `existsSync('foo/bar.md')` 会命中 `foo/Bar.md` 自己——那条断言在 Windows 上
// 恒假，与实现对不对无关。判据只能是「目录里不存在字面量 bar.md 这个目录项」。
const fooNames = fs.readdirSync(path.join(VAULT, 'foo'))
check('4d.old-name-gone', !fooNames.includes('bar.md'), {
  entries: fooNames.length,
  barLike: fooEntries,
})
check(
  '4e.tree-updated',
  await waitFor(`[...document.querySelectorAll('.tree-row')].some((el) => el.title === 'foo/Bar.md')`, 2500),
  'tree',
)

/* ═══════════════════ 验收 5+6 · 重命名当前打开笔记 ═══════════════════ */

await CTRL_P()
await waitFor(`document.querySelector('.quick-open') !== null`, 1500)
await insertText('foobar')
await sleep(120)
await ENTER()
check(
  '5a.opened',
  await waitFor(`document.querySelector('.breadcrumb')?.textContent === 'notes/foobar.md'`, 4000),
  await ev(`document.querySelector('.breadcrumb')?.textContent ?? ''`),
)

// 光标移到文末，并把视口滚到底：400 行的文档，scrollTop 必然 > 0。
await ev(`document.querySelector('.cm-content').focus()`)
await CTRL_END()
await sleep(200)

const marker = `KI-改名撤销-${STAMP}`
await insertText(marker)
await sleep(120)
const before = await ev(`(() => {
  const scroller = document.querySelector('.cm-scroller')
  return { scrollTop: Math.round(scroller.scrollTop), length: document.querySelector('.cm-content').innerText.length }
})()`)
check('5b.at-document-end', before.scrollTop > 0, before)

// 改名**之前**读一次索引连接的重开次数，作为 7e 的基线。
//
// 为什么不能断言绝对值 `builds === 1`：`builds` 是「本进程为这个库打开过几次
// 索引库」的累计值，而本脚本的删除段已经搜过两轮、又重扫过一次。合并之后
// 绝对值必然是 3 起的——写死 1 等于断言「这个脚本没有前文」，与实现无关。
// 要验的是「改名恰好让索引失效一次」，所以只能验**增量**。
//
// 这一步本身可能把连接重开一次（删除段末尾的 `vault_scan` 让缓存失效了），
// 那就把它算进基线；随后改名再失效一次，第一次搜索再开一次 = 基线 + 1。
const buildsBefore = (await ipc('vault_search', { root: rootShown, query: '长文探针在此', refreshPath: null })).value?.builds

const renamedRel = `notes/foobar-改名-${STAMP}.md`
const menu5 = await openNodeMenu('notes/foobar.md')
check('5c.context-menu', menu5.ok, menu5)
await clickMenuItem('重命名')
const accepted5 = await submitName(`foobar-改名-${STAMP}`, { clearFirst: true })
await waitFor(`document.querySelector('.prompt') === null`, 4000)
check('5d.rename-accepted', accepted5.open === false, accepted5)

// 验收 6：改名时槽位里有 pending → 先落盘，磁盘上是改名前的正文，旧路径不存在。
const renamedAbs = path.join(VAULT, renamedRel)
const afterRename = fs.existsSync(renamedAbs) ? fs.readFileSync(renamedAbs, 'utf8') : ''
check('6a.content-followed-the-rename', afterRename.includes(marker), {
  tail: afterRename.slice(-40),
})
check('6b.old-path-gone', !fs.existsSync(path.join(VAULT, 'notes/foobar.md')), 'notes/foobar.md')
check('6c.no-resurrected-file', !listNotes().includes('notes/foobar.md'), 'listNotes')

// 验收 5：滚动位置、光标偏移、撤销栈。
const after = await ev(`(() => {
  const scroller = document.querySelector('.cm-scroller')
  return { scrollTop: Math.round(scroller.scrollTop), focus: document.activeElement?.className ?? '' }
})()`)
check('5e.scroll-preserved', after.scrollTop === before.scrollTop, {
  before: before.scrollTop,
  after: after.scrollTop,
})

// 光标偏移：在光标处再打一个字。它若跑到文首，磁盘上就会出现在开头而不是文末。
await ev(`document.querySelector('.cm-content').focus()`)
await insertText('Z')
await sleep(2200)
const withZ = fs.readFileSync(renamedAbs, 'utf8')
check('5f.cursor-offset-preserved', withZ.endsWith(`${marker}Z`), {
  tail: withZ.slice(-24),
  length: withZ.length,
})

// 撤销栈：一次 Ctrl+Z 撤掉 Z，再一次撤掉 marker。重建过的编辑器两次都不会有反应。
//
// 断言必须先看「撤销前确实有 Z」：否则一个**空文档**也能让「撤销后没有 Z」成立，
// 而那种假绿正是这条用例要防的（重建编辑器时文档是全量重载的）。
const editorText = () => ev(`document.querySelector('.cm-content').innerText`)
const preUndo = (await editorText()).trimEnd()
const undo0 = preUndo.endsWith(`${marker}Z`)

await CTRL_Z()
await sleep(200)
const undo1 = (await editorText()).trimEnd()
await CTRL_Z()
await sleep(250)
const undo2 = (await editorText()).trimEnd()

check('5g.undo-survives-rename', undo0 && undo1.endsWith(marker) && !undo2.includes(marker), {
  before: preUndo.slice(-28),
  afterFirst: undo1.slice(-28),
  afterSecond: undo2.slice(-28),
})

// 撤销结果也要落盘到**新路径**：光看 DOM 不能排除「编辑器对了、IPC 还指着旧路径」。
await sleep(2200)
const afterUndo = fs.readFileSync(renamedAbs, 'utf8')
check('5h.undo-saved-to-new-path', !afterUndo.includes(marker) && afterUndo.trimEnd().endsWith('长文探针在此'), {
  tail: afterUndo.slice(-24),
  oldPathExists: fs.existsSync(path.join(VAULT, 'notes/foobar.md')),
})

/* ═══════════════════ 验收 7 · 三处同步（树 / breadcrumb / 搜索） ═══════════════════ */

check(
  '7a.breadcrumb',
  (await ev(`document.querySelector('.breadcrumb')?.textContent ?? ''`)) === renamedRel,
  await ev(`document.querySelector('.breadcrumb')?.textContent ?? ''`),
)
check(
  '7b.tree',
  await waitFor(`[...document.querySelectorAll('.tree-row')].some((el) => el.title === ${JSON.stringify(renamedRel)})`, 2500),
  'tree',
)

// 搜索：改名之后第一次搜索必须重建索引（vault_scan 会 invalidate）。
const searchWait = ev(`new Promise((resolve) => {
  window.__KI_S0 = performance.now()
  const tick = () => {
    const items = [...document.querySelectorAll('.search__item')]
    if (items.length > 0) {
      resolve({
        ms: performance.now() - window.__KI_S0,
        paths: items.map((el) => el.dataset.path),
        footer: document.querySelector('.search__footer')?.textContent ?? '',
      })
    } else requestAnimationFrame(tick)
  }
  tick()
})`)
await CTRL_SHIFT_F()
await waitFor(`document.querySelector('.search__input') !== null`, 1500)
await insertText('长文探针在此')
const searched = await searchWait

// 第二次搜索：索引已经在缓存里，`index_ms` 必须回到 0（footer 不再显示毫秒），
// 且 builds 仍是 1。它与 7e 一起证明「一次重扫只让索引失效一次」，
// 而不是「每次搜索都重建」——后者在 1000 篇上就是每次多等一个全库读盘。
//
// 必须在关浮层**之前**做：关掉之后 `.search__footer` 就不存在了，
// 那个等待条件永远不成立（第一版把它写在 ESC 之后，于是恒超时）。
const footer1 = searched.footer
const warmWait = ev(`Promise.race([
  new Promise((resolve) => {
    window.__KI_S1 = performance.now()
    const tick = () => {
      const footer = document.querySelector('.search__footer')?.textContent ?? ''
      // 等的是**结果**那一版 footer，不是中间的「搜索中…」：
      // 只有结果里的「建索引 N 次」与缺席的毫秒数才是缓存命中的证据。
      if (footer.includes('建索引') && footer !== ${JSON.stringify(searched.footer)}) {
        resolve({ ms: performance.now() - window.__KI_S1, footer })
      } else requestAnimationFrame(tick)
    }
    tick()
  }),
  new Promise((resolve) => setTimeout(() => resolve(null), 5000)),
])`)
await rawKey('Backspace', 'Backspace', 8) // 长文探针在此 → 长文探针在，仍能命中
const warm = await warmWait
check('10c.second-search-uses-cache', warm !== null && !/\d+\s*ms/.test(warm.footer), warm ?? footer1)

await ESC()
await waitFor(`document.querySelector('.search-panel') === null`, 1500)

check('7c.hits-new-path', searched.paths.includes(renamedRel), searched.paths.slice(0, 5))
check('7d.no-hits-old-path', !searched.paths.includes('notes/foobar.md'), searched.paths.slice(0, 5))

const indexMs = Number((/(\d+)\s*ms/.exec(searched.footer) ?? [])[1] ?? -1)
const builds = Number((/建索引 (\d+) 次/.exec(searched.footer) ?? [])[1] ?? -1)
check('7e.index-rebuilt-once', builds === buildsBefore + 1, {
  buildsBefore,
  builds,
  footer: searched.footer,
})

// 10b 的口径：改名 → vault_scan → search::invalidate → 下一次搜索重建索引。
// 这里只断言「重建确实发生了、耗时被量到、没有数量级回归」，**不断言 1.4s 这类数字**：
// 这套夹具里刻意塞了 2MB 的 bad/huge-note.md 与 GBK 文件（验收 7 要用），
// 冷启动建索引的真实数字随磁盘缓存波动很大，报告里给实测值而不是把它变成脆断言。
// 真正的性能承诺（干净 1000 篇 ≈1.4s）由报告里的对照测量负责。
check('10b.rename-first-search', builds === buildsBefore + 1 && indexMs >= 0 && indexMs < 15000, {
  buildsBefore,
  wallMs: Math.round(searched.ms),
  indexMs,
  footer: searched.footer,
})

/* ═══════════════════ 验收 8 · 非法名 7 例 ═══════════════════ */

const before8 = listNotes()
const illegal = [
  { input: 'CON', needle: '保留' },
  { input: 'a:b', needle: '不能有 :' },
  { input: 'foo.', needle: '点结尾' },
  { input: 'foo ', needle: '空白' },
  { input: '', needle: '不能为空' },
  { input: '../x', needle: '斜杠' },
  { input: 'a'.repeat(300), needle: '太长' },
]

const illegalDetails = []
for (const item of illegal) {
  if (!(await openCreatePrompt())) {
    illegalDetails.push({ input: item.input.slice(0, 12), ok: false, reason: '浮层没打开' })
    continue
  }
  const state = await submitName(item.input)
  const text = state.errors.join(' | ')
  illegalDetails.push({
    input: item.input.length > 12 ? `${item.input.slice(0, 6)}…(${item.input.length})` : item.input,
    ok: state.open === true && text.includes(item.needle),
    text,
  })
  await closePromptIfOpen()
}
const after8 = listNotes()
check(
  '8a.all-seven-rejected',
  illegalDetails.every((d) => d.ok),
  illegalDetails,
)
check('8b.no-new-file', after8.length === before8.length && after8.every((p, i) => p === before8[i]), {
  before: before8.length,
  after: after8.length,
})

// 8c：绕过前端直接打 IPC。前端拦不住的入口（将来的 isle CLI）必须被 Rust 拦住。
const direct = await ev(`window.__TAURI_INTERNALS__
  .invoke('note_create', { root: ${JSON.stringify(VAULT)}, dir: '', name: 'CON' })
  .then(() => null, (e) => e)`)
check(
  '8c.rust-is-the-gate',
  direct !== null && direct.code === 'INVALID_NAME' && String(direct.detail?.reason ?? '').includes('保留'),
  direct,
)

/* ═══════════════════ 验收 9 · 全角 .ｍｄ ═══════════════════ */

const before9 = listNotes()
const fw = await (async () => {
  await openCreatePrompt()
  const state = await submitName(`全角-${STAMP}.ｍｄ`)
  await closePromptIfOpen()
  return state
})()
check(
  '9a.rejected-with-readable-reason',
  fw.open === true && fw.errors.join('').includes('全角'),
  fw.errors,
)
check('9b.no-file-on-disk', listNotes().length === before9.length, {
  before: before9.length,
  after: listNotes().length,
})

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
  path.join(OUT_DIR, `tree-acceptance-${STAMP}.json`),
  JSON.stringify({ results, consoleErrors, stamp: STAMP }, null, 2),
)
app.close()
process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
