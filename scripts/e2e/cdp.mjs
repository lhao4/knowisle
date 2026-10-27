// CDP 通道：启停应用、连 target、evaluate、等条件、合成输入。
//
// # 为什么单独一个文件
//
// B-33 要搬的九个脚本里有七份几乎逐字相同的 `connect()` / `launch()` /
// `press()`。抄出来的第二份一定会漂：`connect()` 该不该按 URL 挑 target、
// `waitFor` 要不要吞异常、`killApp` 之后要不要等端口释放——每一条都是踩过的
// 坑（TOOLING「CDP 验收手法」）。收口到一处，改一次全体生效。
//
// # 两种启动模式
//
// - 默认**attach**：连已经在跑的应用。连不上就抛，绝不"空跑出一片绿"（B-19）。
// - `--restart` / `KI_E2E_RESTART=1`：先 `taskkill` 再起。`e2e:all` 用这一种，
//   因为调试槽位是进程级静态（B-16）：同一进程里跑第二个脚本，探针会重复注入。
//
// 只支持 Windows：启动参数走 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`，
// 杀进程走 `taskkill`。Linux 的 WebKitGTK 是另一套 inspector 协议，没验过。
import { execFileSync, spawn } from 'node:child_process'
import fs from 'node:fs'

import { APP_EXE, APP_ORIGIN, CDP_PORT, RESTART, sleep } from './env.mjs'

export const MOD = { alt: 1, ctrl: 2, meta: 4, shift: 8 }

const PORT_HINT = (port) =>
  `连不上 127.0.0.1:${port}：前置没满足。应用必须带 --remote-debugging-port=${port} 启动，` +
  `且 debug 版连的是 ${APP_ORIGIN}（先 pnpm dev 起 vite）。` +
  `也可以加 --restart 让脚本自己杀掉并重启应用。`

function assertWindows() {
  if (process.platform !== 'win32') {
    throw new Error('这些脚本只支持 Windows：启动参数走 WebView2，杀进程走 taskkill')
  }
}

/** debug 产物不存在时立刻失败，而不是等到"连不上 9222"再猜。 */
export function assertDebugBuild() {
  if (!fs.existsSync(APP_EXE)) {
    throw new Error(
      `debug 产物不存在：${APP_EXE}\n` +
        '先 `cargo build -p knowisle`（或 `pnpm dev`）把应用编出来。',
    )
  }
}

/**
 * vite 必须在 1420 上**真的下发模块**。
 *
 * 只探 `/` 不够：dev 页面可以是 200 而模块 502（系统代理）或旧 transform
 * （B-18）。这里取一个真实模块，确认它被转译过（带 `import`）。
 */
export async function assertVite(timeoutMs = 3000) {
  let res
  try {
    res = await fetch(`${APP_ORIGIN}/src/App.tsx`, { signal: AbortSignal.timeout(timeoutMs) })
  } catch (err) {
    throw new Error(`vite 不在 ${APP_ORIGIN}：先 \`pnpm dev\`（${err.message}）`)
  }
  if (!res.ok) throw new Error(`vite 在 ${APP_ORIGIN} 但 /src/App.tsx 返回 ${res.status}`)
  const body = await res.text()
  if (!body.includes('import')) {
    throw new Error(`vite 下发的 /src/App.tsx 不像转译结果：检查 dev server 是否换了根目录`)
  }
}

export function killApp() {
  assertWindows()
  try {
    execFileSync('taskkill', ['/F', '/IM', 'knowisle.exe'], { stdio: 'ignore' })
  } catch {
    // 本来就没在跑
  }
}

export async function waitPortClosed(port = CDP_PORT, attempts = 150) {
  for (let i = 0; i < attempts; i += 1) {
    try {
      await fetch(`http://127.0.0.1:${port}/json/version`, { signal: AbortSignal.timeout(1000) })
      await sleep(100)
    } catch {
      return
    }
  }
  throw new Error(`调试端口 ${port} 一直没释放：上一轮的 knowisle.exe 还在？`)
}

/** attach 模式的前置检查：端口必须现在就通，不轮询。 */
async function assertPort(port) {
  try {
    const res = await fetch(`http://127.0.0.1:${port}/json/version`, {
      signal: AbortSignal.timeout(1500),
    })
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
  } catch {
    throw new Error(PORT_HINT(port))
  }
}

function spawnApp(port) {
  spawn(APP_EXE, [], {
    detached: true,
    stdio: 'ignore',
    env: {
      ...process.env,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--no-sandbox --remote-debugging-port=${port}`,
    },
  }).unref()
}

/**
 * 连上 dev 页面的 target。
 *
 * `/json` 里可能同时有 `about:blank` 的僵尸与好的 target。优先挑 URL 落在
 * dev 地址上的那个；一个都没有时退到第一个 page（刚启动时它还没导航），
 * 再由调用方用 `location.href` 判真伪——**不能信 `/json` 的 url 字段**
 * （vite 死了之后它仍报 dev 地址）。
 */
export async function connect({ port = CDP_PORT, errors = [], onEvent, timeoutMs = 45000 } = {}) {
  const deadline = Date.now() + timeoutMs
  let page = null
  while (Date.now() < deadline) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json()
      const pages = list.filter((t) => t.type === 'page')
      page = pages.find((t) => (t.url ?? '').startsWith(APP_ORIGIN)) ?? pages[0] ?? null
      if (page) break
    } catch {
      // 还没起来
    }
    await sleep(150)
  }
  if (page === null) throw new Error(PORT_HINT(port))

  const ws = new WebSocket(page.webSocketDebuggerUrl)
  let seq = 0
  const waiting = new Map()

  const opened = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('CDP WebSocket 5 秒没打开')), 5000)
    ws.addEventListener('open', () => {
      clearTimeout(timer)
      resolve()
    })
  })

  ws.addEventListener('message', (e) => {
    const m = JSON.parse(e.data)
    if (m.id && waiting.has(m.id)) {
      waiting.get(m.id)(m)
      waiting.delete(m.id)
      return
    }
    // 事件先交给调用方：大纲验收要数 console.count，那是它自己的判据。
    onEvent?.(m)
    if (m.method === 'Runtime.consoleAPICalled' && (m.params.type === 'error' || m.params.type === 'assert')) {
      errors.push(m.params.args.map((a) => a.value ?? a.description ?? a.type).join(' '))
    }
    if (m.method === 'Runtime.exceptionThrown') {
      const d = m.params.exceptionDetails
      errors.push(d.exception?.description ?? d.text)
    }
  })

  const send = (method, params = {}) =>
    new Promise((resolve) => {
      const id = ++seq
      waiting.set(id, resolve)
      ws.send(JSON.stringify({ id, method, params }))
    })

  await opened
  await send('Runtime.enable')
  await send('Page.enable')

  const ev = async (expression) => {
    const res = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true })
    if (res.result?.exceptionDetails) {
      throw new Error(res.result.exceptionDetails.exception?.description ?? 'evaluate failed')
    }
    return res.result?.result?.value
  }

  /** 轮询直到表达式为真。超时返回 false（判据自己去断言，不在这里抛）。 */
  const waitFor = async (expression, timeoutMs = 4000) => {
    const deadline = Date.now() + timeoutMs
    while (Date.now() < deadline) {
      try {
        if (await ev(expression)) return true
      } catch {
        // 重载 / 上下文切换期间 evaluate 会短暂失败，等下一轮
      }
      await sleep(40)
    }
    return false
  }

  const press = async (key, code, vk, modifiers = 0) => {
    const base = { key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk, modifiers }
    await send('Input.dispatchKeyEvent', { type: 'keyDown', ...base })
    await send('Input.dispatchKeyEvent', { type: 'keyUp', ...base })
  }

  /**
   * 需要**浏览器默认编辑行为**的键（Backspace / Delete）必须发 `rawKeyDown`。
   * 用 `keyDown` 只派发 keydown，不删字符——不报错，只是"什么都没发生"。
   */
  const rawKey = async (key, code, vk, modifiers = 0) => {
    await send('Input.dispatchKeyEvent', {
      type: 'rawKeyDown',
      key,
      code,
      windowsVirtualKeyCode: vk,
      nativeVirtualKeyCode: vk,
      modifiers,
    })
    await send('Input.dispatchKeyEvent', { type: 'keyUp', key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk, modifiers })
  }

  const insertText = async (text, settle = 20) => {
    await send('Input.insertText', { text })
    await sleep(settle)
  }

  const clickAt = async (x, y, settle = 120) => {
    await send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', buttons: 1, clickCount: 1 })
    await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', buttons: 0, clickCount: 1 })
    await sleep(settle)
  }

  const rightClickAt = async (x, y, settle = 150) => {
    await send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'right', buttons: 2, clickCount: 1 })
    await send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'right', buttons: 0, clickCount: 1 })
    await sleep(settle)
  }

  /** 元素中心点；元素不存在返回 null。 */
  const centerOf = (selector) =>
    ev(`(() => {
      const el = document.querySelector(${JSON.stringify(selector)})
      if (el === null) return null
      const r = el.getBoundingClientRect()
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 }
    })()`)

  /** 点一个真实 DOM 节点（真鼠标事件，顺带验命中测试没被浮层挡住）。 */
  const clickSelector = async (selector) => {
    const rect = await centerOf(selector)
    if (rect === null) return false
    await clickAt(rect.x, rect.y)
    return true
  }

  const waitHref = async (timeoutMs = 25000) => {
    if (await waitFor(`location.href.startsWith(${JSON.stringify(APP_ORIGIN)})`, timeoutMs)) return
    const href = await ev('location.href').catch(() => '（读不到）')
    throw new Error(
      `页面停在 ${href}，没到 ${APP_ORIGIN}：vite 是不是没起？` +
        '（判据是 location.href，不是 /json 的 url 字段——后者会撒谎）',
    )
  }

  return {
    ws,
    send,
    ev,
    waitFor,
    errors,
    press,
    rawKey,
    insertText,
    clickAt,
    rightClickAt,
    centerOf,
    clickSelector,
    waitHref,
    reload: async () => {
      await send('Page.reload', {})
      await waitHref()
    },
    close: () => {
      try {
        ws.close()
      } catch {
        // 已经断了
      }
    },
  }
}

/**
 * 打开（或连上）应用，把夹具库装上，等界面就绪。
 *
 * # 夹具在应用"没有库"的时候建
 *
 * 夹具会删 `.md`、清回收目录。应用若正开着同一个库、编辑器里还有未落盘的
 * 改动，1.5 秒后的自动保存会把刚删掉的文件写回来——断言于是红在一个完全
 * 正确的实现上。所以两种模式都先让应用进入"没有库"的状态，再建夹具：
 *
 * - `restart`：先杀掉进程（进程没了，句柄自然没了），建完再起。
 * - `attach`：先连上去把 `lastVault` 清掉并 reload 到欢迎页，再建夹具。
 *   这也是"关掉应用跑 `pnpm e2e:trash` 必须报错"的那条判据：端口不通就抛，
 *   不会自己偷偷把应用拉起来（B-19）。
 *
 * @param vault   夹具库绝对路径；`null` = 显式清掉 lastVault（冷启动用例）；
 *                不传 = 不动 localStorage（应用自己恢复到上次的库）。
 * @param ready   就绪判据（CSS 选择器）。树用 `.tree-row`，欢迎页用 `.welcome`。
 * @param prepare 建夹具的回调，在应用处于"没有库"的状态时调用。
 * @param errors  控制台错误收集数组。跨重启要复用同一个数组，所以由调用方持有。
 */
export async function openApp({
  vault,
  ready = '.tree-row',
  port = CDP_PORT,
  restart = RESTART,
  prepare,
  errors = [],
  onEvent,
  timeoutMs = 60000,
} = {}) {
  assertWindows()
  assertDebugBuild()
  await assertVite()

  if (restart) {
    killApp()
    await waitPortClosed(port)
    if (prepare) await prepare()
    spawnApp(port)
  } else {
    await assertPort(port)
    // 先让应用回到"没有库"的状态，再动磁盘。用完就断，下面重新连一个干净的会话。
    const pre = await connect({ port, errors: [] })
    await pre.waitHref(25000)
    await pre.ev(`localStorage.removeItem('knowisle.lastVault'); true`)
    await pre.reload()
    if (!(await pre.waitFor(`document.querySelector('.welcome') !== null`, 15000))) {
      throw new Error('清掉 lastVault 之后没回到欢迎页：应用可能正开着别的库，先重启它')
    }
    pre.close()
    if (prepare) await prepare()
  }

  const session = await connect({ port, errors, onEvent })
  await session.waitHref(25000)

  if (vault !== undefined) {
    const expr =
      vault === null
        ? `localStorage.removeItem('knowisle.lastVault'); true`
        : `localStorage.setItem('knowisle.lastVault', ${JSON.stringify(vault)}); true`
    await session.ev(expr)
    await session.reload()
  }

  if (ready !== null) {
    const ok = await session.waitFor(`document.querySelector(${JSON.stringify(ready)}) !== null`, timeoutMs)
    if (!ok) throw new Error(`等 ${ready} 超时：夹具库没打开 / 树没渲染出来（vite 或库路径有问题）`)
  }

  // 重载之前的错误不算：Runtime.enable 会重放旧消息（B-19 的假绿路径）。
  errors.length = 0
  return session
}
