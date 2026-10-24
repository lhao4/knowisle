#!/usr/bin/env node
/**
 * M1 三项性能验收的测量脚本。
 *
 * | 场景 | 指标 | 标准 |
 * |---|---|---|
 * | all（默认） | 10 万字文档滚动 | 帧时间 p95 ≤ 16.7ms |
 * | all | 击键到像素 | ≤ 16ms |
 * | all | 保存 100 次 | 无内存增长 |
 * | outline | 500 个标题 + 大纲面板，滚动 300 帧 | 相对空闲基线不掉帧 |
 *
 * # 为什么必须跑在 Tauri 里
 *
 * 浏览器标签页与 WebView2 的渲染路径不同（合成器、字体回退、DPR 都不一样），
 * 在 Chrome 里测出来的数字不能替 WebView2 背书。
 *
 * # 前置
 *
 * 1. `pnpm dev`（debug 版连的是 devUrl，不起 vite 就是白页）
 * 2. 应用带远程调试端口启动：
 *    `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222" ./target/debug/knowisle.exe`
 * 3. 运行：`node scripts/perf-m1.mjs --vault <绝对路径> [--scenario all|outline] [--port 9222] [--out result.json]`
 *
 * **每次测量前重启应用。** Rust 侧的自动保存槽位是进程级静态变量，
 * 同一个 `(root, path)` 的乐观锁基线只在"换文档"时被重置；上一个进程留下的
 * 基线配上被重新生成过的夹具，第一次 `Ctrl+S` 必然报 CONFLICT（这本身是正确行为）。
 *
 * # outline 场景为什么单独跑
 *
 * 它要的是「500 个标题 + 右侧大纲面板」时的滚动帧时间。与 all 场景共用夹具生成、
 * CDP 连接、帧统计三块代码，但**不跑**击键与保存——那两项在 500 标题的文档上
 * 没有意义，只会把一次测量拖到十几分钟。
 *
 * # 三条已知局限（结论里必须一起读）
 *
 * 1. **帧时间是 rAF 间隔，不是上屏时间。** rAF 回调发生在样式/布局之后、绘制之前，
 *    所以它量到的是"这一帧的主线程有没有赶上垂直同步"，不含合成与上屏延迟。
 *    空闲时的基线也会给出——它才是这台机器的帧节奏，对比才有意义。
 * 2. **击键是 CDP 合成事件。** 真实按键还要经过 OS 输入队列与 WebView2 的
 *    IPC，这段不在测量范围内，所以数字是**下界**；而"到下一帧"又包含等垂直同步
 *    的时间，所以同一个数字也是**上界**。两个方向都说了，读者自己判断。
 * 3. **`--disable-gpu` 时是软件光栅。** 该参数在无显卡/远程会话里是必需的，
 *    但会让滚动显著变慢。`--gpu` 一栏才是普通用户的路径。
 */
import fs from 'node:fs'
import path from 'node:path'
import { execFileSync } from 'node:child_process'

const args = process.argv.slice(2)
const argOf = (name, fallback) => {
  const i = args.indexOf(`--${name}`)
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback
}
const VAULT = argOf('vault', process.cwd())
const PORT = Number(argOf('port', '9222'))
const OUT = argOf('out', '')
const LABEL = argOf('label', 'run')
/** `all`（默认）跑 M1 三项；`outline` 只跑大纲场景。 */
const SCENARIO = argOf('scenario', 'all')
const OUTLINE_ONLY = SCENARIO === 'outline'
/** 大纲场景的标题数。500 是本轮的验收口径。 */
const OUTLINE_HEADINGS = 500

// ── 夹具 ──────────────────────────────────────────────────────────────
// 每次都重写：上一轮保存测试会在文档里留下 100 个 `x`，不重写的话每跑一次
// 夹具就长一点，两次测量不可比。重写会改 mtime，所以**必须**在应用刚启动、
// 槽位为空时跑（见文件头的前置说明）。
const FIXTURE = path.join(VAULT, OUTLINE_ONLY ? 'perf-outline.md' : 'perf-100k.md')
if (OUTLINE_ONLY) {
  // 500 个标题，混合两级，每个标题下面都有一段正文：空标题会让大纲面板
  // 很矮、根本不需要滚动，测不到滚动同步的真实成本。
  let doc = '# 五百个标题的性能样本\n\n'
  for (let i = 1; i <= OUTLINE_HEADINGS; i += 1) {
    const level = i % 10 === 1 ? '#' : '##'
    doc += `${level} 第 ${i} 节 · 章节标题章节标题\n\n`
    doc += `这一节只有一段正文，用来给标题之间撑出高度。${'知识管理的第一步不是收集，而是丢弃。'.repeat(2)}\n\n`
  }
  fs.writeFileSync(FIXTURE, doc, 'utf8')
} else {
  const para =
    '知识管理的第一步不是收集，而是丢弃。本地优先意味着磁盘上的 Markdown 是唯一真相源，索引只是可重建的派生数据。'
  let doc = '# 十万字性能样本\n\n'
  let i = 0
  while (doc.length < 100_000) {
    i += 1
    doc += `## 第 ${i} 节\n\n${para}${para}\n\nThe quick brown fox jumps over the lazy dog. ${i} — ${'lorem ipsum dolor sit amet '.repeat(2)}\n\n- 条目 A${i}\n- 条目 B${i}\n\n`
  }
  fs.writeFileSync(FIXTURE, doc, 'utf8')
}
const FIXTURE_CHARS = fs.readFileSync(FIXTURE, 'utf8').length
console.log(`夹具：${FIXTURE}（${FIXTURE_CHARS} 字符）`)

// ── CDP ───────────────────────────────────────────────────────────────
const targets = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json()
const page = targets.find((t) => t.type === 'page')
if (!page) throw new Error('没找到 page target：应用是不是没带 --remote-debugging-port 启动？')
const ws = new WebSocket(page.webSocketDebuggerUrl)
let seq = 0
const waiting = new Map()
ws.addEventListener('message', (e) => {
  const m = JSON.parse(e.data)
  if (m.id && waiting.has(m.id)) {
    waiting.get(m.id)(m)
    waiting.delete(m.id)
  }
})
const send = (method, params = {}) =>
  new Promise((resolve) => {
    const id = ++seq
    waiting.set(id, resolve)
    ws.send(JSON.stringify({ id, method, params }))
  })
await new Promise((r) => ws.addEventListener('open', r))
await send('Runtime.enable')
await send('Page.enable')
await send('HeapProfiler.enable').catch(() => {})

const ev = async (expression) => {
  const res = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true })
  if (res.result?.exceptionDetails) {
    throw new Error(res.result.exceptionDetails.exception?.description ?? 'evaluate failed')
  }
  return res.result?.result?.value
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

const heap = async () => {
  await send('HeapProfiler.collectGarbage').catch(() => {})
  const r = await send('Runtime.getHeapUsage')
  return r.result?.usedSize ?? null
}
const workingSet = (name) => {
  try {
    const out = execFileSync(
      'powershell',
      ['-NoProfile', '-Command', `(Get-Process -Name ${name} -ErrorAction SilentlyContinue | Measure-Object WorkingSet64 -Sum).Sum`],
      { encoding: 'utf8' },
    ).trim()
    return Number(out) || 0
  } catch {
    return 0
  }
}

// ── 打开 Vault 与目标笔记 ─────────────────────────────────────────────
// 原生对话框无法从 CDP 驱动（Tauri 2 的 __TAURI_INTERNALS__.invoke 是 non-writable），
// 所以写「上次打开的库」再 reload，走应用自己的启动恢复路径。
await ev(`localStorage.setItem('knowisle.lastVault', ${JSON.stringify(VAULT)})`)
await send('Page.reload', {})
await sleep(2500)

const openNote = async (name) => {
  const ok = await ev(`(() => {
    const row = [...document.querySelectorAll('.tree-row')].find((r) => r.innerText.includes(${JSON.stringify(name)}))
    if (!row) return false
    row.click()
    return true
  })()`)
  await sleep(1500)
  return ok
}
const FIXTURE_NAME = path.basename(FIXTURE)
if (!(await openNote(FIXTURE_NAME))) throw new Error(`树里没有 ${FIXTURE_NAME}`)

const docChars = await ev(`document.querySelector('.cm-content').innerText.length`)
console.log(`已打开：${FIXTURE_NAME}（磁盘 ${FIXTURE_CHARS} 字符；DOM 内已渲染 ${docChars} 字符——CM6 只渲染视口）`)

const result = {
  label: LABEL,
  fixtureChars: FIXTURE_CHARS,
  // 软件光栅（SwiftShader / Basic Render Driver）与真显卡的滚动数字完全不同，
  // 报告里必须带上这一行，否则数字不可解释。
  renderer: await ev(`(() => {
    const gl = document.createElement('canvas').getContext('webgl')
    if (!gl) return 'no-webgl'
    const ext = gl.getExtension('WEBGL_debug_renderer_info')
    return ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER)
  })()`),
  refresh: null,
  scroll: null,
  keystroke: null,
  memory: null,
}
console.log('渲染后端   :', result.renderer)

const measureFrames = async (mode, frames) =>
  ev(`new Promise((resolve) => {
    const scroller = document.querySelector('.cm-scroller')
    scroller.scrollTop = 0
    const intervals = []
    const loaf = []
    let lo = null
    try {
      lo = new PerformanceObserver((list) => {
        for (const e of list.getEntries()) loaf.push(Math.round(e.duration))
      })
      lo.observe({ type: 'long-animation-frame' })
    } catch (err) { lo = null }
    const max = scroller.scrollHeight - scroller.clientHeight
    let last = 0
    let n = 0
    const step = (t) => {
      if (last !== 0) intervals.push(t - last)
      last = t
      n += 1
      if (n > ${frames}) {
        if (lo) lo.disconnect()
        resolve({ intervals, loaf, scrollTop: scroller.scrollTop, scrollHeight: scroller.scrollHeight, loafSupported: lo !== null })
        return
      }
      if (${JSON.stringify(mode)} === 'scroll') scroller.scrollTop += 40
      else if (${JSON.stringify(mode)} === 'jump') scroller.scrollTop = (n * 997) % max
      requestAnimationFrame(step)
    }
    requestAnimationFrame(step)
  })`)

const stats = (xs) => {
  const s = [...xs].sort((a, b) => a - b)
  const q = (p) => s[Math.min(s.length - 1, Math.floor((s.length - 1) * p))]
  return {
    n: s.length,
    p50: +q(0.5).toFixed(2),
    p95: +q(0.95).toFixed(2),
    p99: +q(0.99).toFixed(2),
    max: +s[s.length - 1].toFixed(2),
    over20: s.filter((x) => x > 20).length,
    over33: s.filter((x) => x > 33.4).length,
  }
}

if (!OUTLINE_ONLY) {
  // ── ① 滚动 ────────────────────────────────────────────────────────────
  // 先量空闲基线：它才是这台机器/这个 WebView 的帧节奏，用来解释滚动数字。
  //
  // 两种滚动模式：
  //   scroll = 每帧 40px，模拟滚轮/触控板的连续滚动（视口每帧移动一两行）
  //   jump   = 每帧跳到远处，最坏情况（视口内容整屏更换 + 装饰全部重建）
  const idle = await measureFrames('idle', 180)
  result.refresh = stats(idle.intervals)
  console.log('空闲帧节奏 :', JSON.stringify(result.refresh))

  const scroll = await measureFrames('scroll', 300)
  result.scroll = {
    ...stats(scroll.intervals),
    loaf: scroll.loaf,
    loafSupported: scroll.loafSupported,
    scrolledTo: Math.round(scroll.scrollTop),
    scrollHeight: Math.round(scroll.scrollHeight),
  }
  console.log('滚动 40px/帧:', JSON.stringify(result.scroll))

  const jump = await measureFrames('jump', 200)
  result.jumpScroll = {
    ...stats(jump.intervals),
    loaf: jump.loaf,
    scrolledTo: Math.round(jump.scrollTop),
  }
  console.log('跳滚 最坏情况:', JSON.stringify(result.jumpScroll))

  // ── ② 击键到像素 ──────────────────────────────────────────────────────
  // T0 = keydown 捕获阶段；T_dom = MutationObserver 微任务（CM6 已改完 DOM）；
  // T_frame = keydown 里排的 rAF（这一帧绘制前）。
  await ev(`(() => {
    window.__KEY__ = []
    window.__KEY_MO__ = new MutationObserver(() => {
      const rec = window.__KEY__[window.__KEY__.length - 1]
      if (rec && rec.dom === null) rec.dom = performance.now()
    })
    window.__KEY_MO__.observe(document.querySelector('.cm-content'), {
      childList: true, characterData: true, subtree: true,
    })
    window.addEventListener('keydown', (e) => {
      if (e.key.length !== 1) return
      const rec = { t0: performance.now(), dom: null, frame: null, key: e.key }
      window.__KEY__.push(rec)
      requestAnimationFrame((t) => { rec.frame = t })
    }, true)
    document.querySelector('.cm-content').focus()
    return 'armed'
  })()`)

  const KEYS = 60
  for (let i = 0; i < KEYS; i += 1) {
    await send('Input.dispatchKeyEvent', {
      type: 'keyDown', key: 'a', code: 'KeyA', windowsVirtualKeyCode: 65, nativeVirtualKeyCode: 65, text: 'a', unmodifiedText: 'a',
    })
    await send('Input.dispatchKeyEvent', {
      type: 'keyUp', key: 'a', code: 'KeyA', windowsVirtualKeyCode: 65, nativeVirtualKeyCode: 65,
    })
    await sleep(35)
  }
  await sleep(300)

  const keyRaw = await ev(`window.__KEY__`)
  const domDeltas = keyRaw.filter((r) => r.dom !== null).map((r) => r.dom - r.t0)
  const frameDeltas = keyRaw.filter((r) => r.frame !== null).map((r) => r.frame - r.t0)
  result.keystroke = {
    samples: keyRaw.length,
    dom: stats(domDeltas),
    frame: stats(frameDeltas),
  }
  console.log('击键 → DOM :', JSON.stringify(result.keystroke.dom))
  console.log('击键 → 帧  :', JSON.stringify(result.keystroke.frame))

  // 撤销掉刚才敲的字，别把夹具改了
  for (let i = 0; i < KEYS; i += 1) {
    await send('Input.dispatchKeyEvent', {
      type: 'keyDown', key: 'z', code: 'KeyZ', windowsVirtualKeyCode: 90, nativeVirtualKeyCode: 90, modifiers: 2,
    })
    await send('Input.dispatchKeyEvent', {
      type: 'keyUp', key: 'z', code: 'KeyZ', windowsVirtualKeyCode: 90, nativeVirtualKeyCode: 90, modifiers: 2,
    })
    await sleep(12)
  }
  await sleep(600)
  console.log('撤销后字符数 :', await ev(`document.querySelector('.cm-content').innerText.length`))

  // ── ③ 保存 100 次 ─────────────────────────────────────────────────────
  // 每次「敲一个字 + Ctrl+S」= 一次真实写盘（含 fsync）。
  //
  // 跑 3 轮而不是 1 轮：单轮涨了 300KB 时，无法区分"泄漏"与"撤销栈多了一条记录"。
  // 泄漏是线性的，撤销栈会封顶（CM6 默认最多留 500 条）。看第二轮、第三轮的增量。
  const ROUNDS = Number(argOf('rounds', '3'))
  const SAVES_PER_ROUND = 100
  const originalBytes = fs.readFileSync(FIXTURE)
  const mtimeBefore = fs.statSync(FIXTURE).mtimeMs
  const rounds = []
  const rssBefore = workingSet('knowisle')
  const webviewBefore = workingSet('msedgewebview2')
  let heapBefore = await heap()

  for (let r = 1; r <= ROUNDS; r += 1) {
    for (let i = 0; i < SAVES_PER_ROUND; i += 1) {
      await send('Input.insertText', { text: 'x' })
      await send('Input.dispatchKeyEvent', {
        type: 'keyDown', key: 's', code: 'KeyS', windowsVirtualKeyCode: 83, nativeVirtualKeyCode: 83, modifiers: 2,
      })
      await send('Input.dispatchKeyEvent', {
        type: 'keyUp', key: 's', code: 'KeyS', windowsVirtualKeyCode: 83, nativeVirtualKeyCode: 83, modifiers: 2,
      })
      await sleep(40)
    }
    await sleep(800)
    const heapAfter = await heap()
    rounds.push({
      round: r,
      saves: SAVES_PER_ROUND,
      heapBefore,
      heapAfter,
      heapDelta: heapAfter !== null && heapBefore !== null ? heapAfter - heapBefore : null,
    })
    console.log(`  第 ${r} 轮 100 次保存：heap ${heapBefore} → ${heapAfter}（Δ ${rounds.at(-1).heapDelta}）`)
    heapBefore = heapAfter
  }

  const rssAfter = workingSet('knowisle')
  const webviewAfter = workingSet('msedgewebview2')
  result.memory = {
    rounds,
    totalSaves: ROUNDS * SAVES_PER_ROUND,
    jsHeapBefore: rounds[0].heapBefore,
    jsHeapAfter: rounds.at(-1).heapAfter,
    jsHeapDelta: rounds.at(-1).heapAfter - rounds[0].heapBefore,
    rustRssBefore: rssBefore,
    rustRssAfter: rssAfter,
    rustRssDelta: rssAfter - rssBefore,
    webviewRssBefore: webviewBefore,
    webviewRssAfter: webviewAfter,
    webviewRssDelta: webviewAfter - webviewBefore,
    fileBytes: fs.statSync(FIXTURE).size,
    mtimeChanged: fs.statSync(FIXTURE).mtimeMs > mtimeBefore,
  }
  console.log('保存汇总    :', JSON.stringify(result.memory, null, 2))

  // 把夹具还原，免得每跑一次长 100 个字符（下一轮测量前会重启应用，基线冲突不存在）。
  fs.writeFileSync(FIXTURE, originalBytes)
}

// ── 大纲场景 ──────────────────────────────────────────────────────────
// 验收：500 个标题的文档，滚动 300 帧，帧时间相对空闲基线不掉帧。
//
// 光标先送到文末：这样「当前章节」是最后一个标题，大纲面板滚到底部，
// 滚动时每帧的 `revealActive` 都有真实对象可操作（虽然多数帧是空操作）。
// 光标停在开头的话面板不动，等于没测到滚动同步。
if (OUTLINE_ONLY) {
  await ev(`document.querySelector('.cm-content').focus()`)
  for (const type of ['keyDown', 'keyUp']) {
    await send('Input.dispatchKeyEvent', {
      type, key: 'End', code: 'End', windowsVirtualKeyCode: 35, nativeVirtualKeyCode: 35, modifiers: 2,
    })
  }
  await sleep(600)

  // 大纲来自增量解析：500 个标题要等后台解析器按片推完（每片 ≤100ms）。
  // 等到 501 项（多出来的是文档首行的大标题）或 15 秒超时；超时也继续测，
  // 但把实际值写进结果——掩盖没解析完才是更坏的选择。
  let seen = 0
  for (let i = 0; i < 150; i += 1) {
    seen = await ev(`document.querySelectorAll('.outline__item').length`)
    if (seen >= OUTLINE_HEADINGS + 1) break
    await sleep(100)
  }

  const panel = await ev(`(() => {
    const list = document.querySelector('.outline__list')
    if (list === null) return null
    return {
      items: list.children.length,
      scrollHeight: Math.round(list.scrollHeight),
      clientHeight: Math.round(list.clientHeight),
      scrollTop: Math.round(list.scrollTop),
      active: document.querySelector('.outline__item.is-active')?.textContent ?? null,
    }
  })()`)
  console.log('大纲面板   :', JSON.stringify(panel))

  // 空闲基线先量：它才是这台机器的帧节奏，滚动数字要和它比才有意义。
  const idleOutline = await measureFrames('idle', 180)
  result.refresh = stats(idleOutline.intervals)
  console.log('空闲帧节奏 :', JSON.stringify(result.refresh))

  const scrollOutline = await measureFrames('scroll', 300)
  result.outline = {
    headings: OUTLINE_HEADINGS,
    panel,
    scroll: {
      ...stats(scrollOutline.intervals),
      scrolledTo: Math.round(scrollOutline.scrollTop),
      scrollHeight: Math.round(scrollOutline.scrollHeight),
      loaf: scrollOutline.loaf,
    },
  }
  console.log('滚动 40px/帧:', JSON.stringify(result.outline.scroll))
}


console.log('\n' + JSON.stringify(result))
if (OUT) {
  fs.writeFileSync(OUT, JSON.stringify(result, null, 2), 'utf8')
  console.log(`结果已写入 ${OUT}`)
}
ws.close()
