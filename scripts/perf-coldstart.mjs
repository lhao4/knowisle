// 冷启动 K2 与空载内存的测量脚本（release 版，Windows）。
//
// 冷启动拆成两个指标，因为它们量的根本不是一回事：
//   K1 窗口首次可见   —— scripts/perf-window-visible.ps1（不带任何 CDP 参数）
//   K2 树渲染完       —— 本脚本（必须带 CDP 才能判断「渲染完了」）
//
// 为什么必须拆：实测一张 165 字节的空白页（没有 React、没有 CodeMirror）
// 从 spawn 到画出第一个 DOM 也要 1.7s。也就是说 K2 里有 1.7s 是 Tauri +
// WebView2 的进程与环境初始化，与我们的代码无关；而 K1 只有 200ms 上下。
// 把两者混成一个数，就永远说不清「变慢了」是变慢在平台还是在产品上。
//
// 前置（**必须用 tauri CLI 构建**）：
//   cd apps/desktop && pnpm exec tauri build --no-bundle --ci
// 用法：
//   node scripts/perf-coldstart.mjs
//   PERF_ROUNDS=5 PERF_K2_MS=2000 node scripts/perf-coldstart.mjs   # CI
//
// # 为什么不能只跑 `cargo build --release`
//
// 实测：纯 `cargo build --release -p knowisle` 产出的二进制**仍然指向 devUrl**
// （窗口标题是 `127.0.0.1`，页面是 `chrome-error://chromewebdata/`），因为
// `tauri-build` 只有在 `tauri build` 的那套环境里才关掉 `dev` cfg、把前端嵌进二进制。
// 所以那个二进制只能证明「编译过了」，不能用来量启动——CI 的 shell job 用它是对的，
// 它本来就只做编译检查。
//
// # 两个指标的口径
//
// **冷启动**：T0 = `spawn()` 前一刻（等价 `CreateProcess`），
// T1 = 「侧栏树已经渲染出来」——`.topbar` 在、`.tree-row` 至少一行、
// `document.readyState === 'complete'`。
//
// 为什么 T1 不是 `.cm-content`（编辑器）出现：这个应用**不记上次打开的笔记**，
// 启动后停在欢迎页，`.cm-content` 要等用户点一篇才会出现。把它当 T1 会测出一个
// 永远不发生的时刻。真正的「点开一篇到能打字」单独作为 T_edit 报，它属于
// 「打开笔记」的开销，不是启动开销。
//
// **空载 RSS**：从 `knowisle.exe` 的 PID 按父子关系**递归**求和。不要用
// `Get-Process msedgewebview2`——那会把本机其它应用的 WebView2 一起算进来，
// 数字虚高且不可复现。
//
// # 为什么预热要用 WM_CLOSE 而不是 taskkill /F
//
// 实测：`taskkill /F` 之后 localStorage 里刚写的 `lastVault` **会丢**
// （下一轮读回 null，应用于是走「首次启动 → 自动弹选择器」，树永远出不来）。
// 向窗口发 WM_CLOSE（= 用户点右上角 ×）能让 WebView2 正常收尾落盘。
// 只有预热这一次写入需要落盘；10 轮测量都只是读，硬杀无所谓。
//
// # 测不到什么
//
// - 窗口真正上屏的时间（只到 DOM 渲染完成，不含 DWM 合成）
// - OS 文件缓存冷启动（不重启机器就做不到，这里测的是「热缓存下的冷进程」）
// - GPU 进程的显存；工作集之和还会重复计入共享 DLL，比任务管理器的私有工作集偏大
import fs from 'node:fs'
import path from 'node:path'
import { execSync, spawn } from 'node:child_process'

const ROOT = path.resolve(import.meta.dirname, '..')
const VAULT = path.join(ROOT, '.tmp', 'vault-perf')
const PORT = 9222
// 全部可调，CI 用环境变量压轮数以省时间；本地默认 10 轮。
const ROUNDS = Number(process.env.PERF_ROUNDS ?? 10)
const COOLDOWN_MS = Number(process.env.PERF_COOLDOWN_MS ?? 1200)
// K2 预算：spawn → 侧栏树渲染完毕。
// 注意这里面有约 1.7s 是 Tauri + WebView2 的进程与环境初始化（实测：一张
// 165 字节的空白页也要 1.7s），所以它卡的是「前端与扫描的回归」，不是「启动有多快」。
// 真正衡量启动速度的是 K1（scripts/perf-window-visible.ps1）。
const LIMIT_MS = Number(process.env.PERF_K2_MS ?? 2000)
// 空载内存只卡我们自己的那部分。WebView2 子树只记基线——它的进程数与显存
// 策略由系统决定，同一份代码换台机器就能差一百兆。
const LIMIT_SELF_MB = Number(process.env.PERF_SELF_MB ?? 40)
const IDLE_SECONDS = Number(process.env.PERF_IDLE_SECONDS ?? 15)

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
const ms = (t0) => Number(process.hrtime.bigint() - t0) / 1e6

const tryRun = (cmd) => {
  try {
    return execSync(cmd, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] })
  } catch {
    return ''
  }
}

/** 找 release 二进制。`cargo build` 产出 `knowisle.exe`，bundler 可能改名成 `KnowIsle.exe`。 */
function findApp() {
  for (const name of ['knowisle.exe', 'KnowIsle.exe']) {
    const p = path.join(ROOT, 'target', 'release', name)
    if (fs.existsSync(p)) return p
  }
  throw new Error(
    '找不到 target/release/knowisle.exe —— 先 cd apps/desktop && pnpm exec tauri build --no-bundle --ci',
  )
}

const killTree = (pid) => tryRun(`taskkill /PID ${pid} /T /F`)
const killAll = () => tryRun('taskkill /F /IM knowisle.exe')

const psRun = (script) => {
  const b64 = Buffer.from(script, 'utf16le').toString('base64')
  return execSync(`powershell -NoProfile -NonInteractive -EncodedCommand ${b64}`, {
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  }).trim()
}

/** 向该 pid 的所有可见窗口发 WM_CLOSE（等价用户点右上角 ×），让 WebView2 把存储落盘。 */
const closeWindow = (pid) =>
  psRun(`
$ProgressPreference = 'SilentlyContinue'
Add-Type -Namespace W -Name U -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
'@
$target = ${pid}
$script:n = 0
$cb = [W.U+EnumWindowsProc]{
  param($h, $l)
  $p = 0
  [void][W.U]::GetWindowThreadProcessId($h, [ref]$p)
  if ($p -eq $target -and [W.U]::IsWindowVisible($h)) {
    [void][W.U]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
    $script:n = $script:n + 1
  }
  return $true
}
[void][W.U]::EnumWindows($cb, [IntPtr]::Zero)
Write-Output $script:n
`)

/* ───────────────────── 极简 CDP ───────────────────── */

const openWs = (url, timeoutMs = 5000) =>
  new Promise((resolve, reject) => {
    const ws = new WebSocket(url)
    let seq = 0
    const waiting = new Map()
    const timer = setTimeout(() => {
      try {
        ws.close()
      } catch {
        // 已经关了
      }
      reject(new Error('WS 打开超时（多半连到了上一轮的僵尸 target）'))
    }, timeoutMs)
    ws.addEventListener('message', (e) => {
      const m = JSON.parse(e.data)
      if (m.id && waiting.has(m.id)) {
        waiting.get(m.id)(m)
        waiting.delete(m.id)
      }
    })
    ws.addEventListener('error', () => {
      clearTimeout(timer)
      reject(new Error('WS 报错'))
    })
    ws.addEventListener('open', () => {
      clearTimeout(timer)
      resolve({
        send: (method, params = {}) =>
          new Promise((res) => {
            const id = ++seq
            waiting.set(id, res)
            ws.send(JSON.stringify({ id, method, params }))
          }),
        close: () => ws.close(),
      })
    })
  })

/** 拿到 page target 的 ws 地址。应用刚起来时调试端口可能还没开，所以要轮询。 */
async function waitForPage(deadline) {
  while (Date.now() < deadline) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json()
      const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
      if (page) return page.webSocketDebuggerUrl
    } catch {
      // 端口还没开
    }
    await sleep(15)
  }
  return null
}

/**
 * 等调试端口关掉。
 *
 * 这一条是必需的：进程被杀之后端口不会立刻释放，此时 `/json/list` 还能拿到
 * **上一轮**的 page target，连上去的 ws 永远不会 open——脚本会在那里静默挂住，
 * 一轮从 3 秒变成 60 秒。
 */
async function waitPortClosed(timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    try {
      await fetch(`http://127.0.0.1:${PORT}/json/list`)
    } catch {
      return true
    }
    await sleep(50)
  }
  return false
}

async function evaluate(cdp, expression) {
  const res = await cdp.send('Runtime.evaluate', { expression, returnByValue: true })
  return res.result?.result?.value ?? null
}

/**
 * 等到文档真的加载了，再动 `localStorage`。
 *
 * 刚连上 target 的那一刻它可能还停在 `about:blank`，此时访问 `localStorage`
 * 会抛 `SecurityError: Access is denied for this document`——而 `Runtime.evaluate`
 * 把异常放在 `exceptionDetails` 里，`returnByValue` 拿到的 `value` 是 undefined，
 * 看起来就像「写入返回了 null」。BACKLOG B-15 记过这一条。
 */
async function waitForDocument(cdp, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const href = await evaluate(cdp, `location.href`).catch(() => null)
    if (typeof href === 'string' && href !== 'about:blank' && href !== '') return href
    await sleep(20)
  }
  throw new Error('文档一直是 about:blank')
}

const PROBE = `(() => ({
  ready: document.readyState === 'complete',
  topbar: !!document.querySelector('.topbar'),
  rows: document.querySelectorAll('.tree-row').length,
}))()`

/** 界面没就绪时的现场快照。没有它，失败信息只剩「没就绪」三个字，无从下手。 */
const DIAG = `(() => ({
  href: location.href,
  readyState: document.readyState,
  welcome: document.querySelector('.welcome')?.innerText.replace(/\\s+/g, ' ').trim() ?? null,
  rows: document.querySelectorAll('.tree-row').length,
  lastVault: (() => { try { return localStorage.getItem('knowisle.lastVault') } catch { return 'THROW' } })(),
  onboarded: (() => { try { return localStorage.getItem('knowisle.onboarded') } catch { return 'THROW' } })(),
}))()`

/* ───────────────────── 夹具 ───────────────────── */

fs.rmSync(VAULT, { recursive: true, force: true })
fs.mkdirSync(path.join(VAULT, '日记'), { recursive: true })
fs.writeFileSync(path.join(VAULT, 'a.md'), '# A\n\n第一篇。\n')
fs.writeFileSync(path.join(VAULT, 'b.md'), '# B\n\n第二篇。\n')
fs.writeFileSync(path.join(VAULT, 'c.md'), '# C\n\n第三篇。\n')
fs.writeFileSync(path.join(VAULT, '日记', 'd.md'), '# D\n\n第四篇。\n')
fs.writeFileSync(path.join(VAULT, '日记', 'e.md'), '# E\n\n第五篇。\n')

const APP = findApp()
const ENV = {
  ...process.env,
  WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--no-sandbox --remote-debugging-port=${PORT}`,
}

const launch = () => spawn(APP, [], { detached: true, stdio: 'ignore', env: ENV })

/** 起一个实例、连上 CDP、等文档就绪。返回 { child, cdp }。 */
async function boot(timeoutMs = 20000) {
  const child = launch()
  const wsUrl = await waitForPage(Date.now() + timeoutMs)
  if (wsUrl === null) {
    killTree(child.pid)
    throw new Error('调试端口没开')
  }
  const cdp = await openWs(wsUrl)
  await cdp.send('Runtime.enable')
  await cdp.send('Page.enable')
  const href = await waitForDocument(cdp)
  return { child, cdp, href }
}

/* ───────────────────── 预热：把 lastVault 写进去并落盘 ───────────────────── */
//
// 不预置的话，HEAD 的首次启动会**自己弹文件夹选择器**（M3 第三刀的行为），
// 于是每一轮测到的都是「对话框弹出来要多久」，而不是「界面就绪要多久」。

killAll()
await waitPortClosed()
await sleep(400)
{
  const { child, cdp, href } = await boot()
  const written = await evaluate(
    cdp,
    `(() => {
      localStorage.setItem('knowisle.lastVault', ${JSON.stringify(VAULT)});
      localStorage.setItem('knowisle.onboarded', '1');
      return JSON.stringify({
        lastVault: localStorage.getItem('knowisle.lastVault'),
        onboarded: localStorage.getItem('knowisle.onboarded'),
      })
    })()`,
  )
  console.log(`  预热写入 ${written}（文档 ${href}）`)
  cdp.close()
  const posted = closeWindow(child.pid)
  await waitPortClosed(6000)
  killTree(child.pid)
  await waitPortClosed()
  await sleep(500)
  console.log(`  预热收尾：WM_CLOSE 发给 ${posted} 个窗口`)
}

// 校验落盘：再起一次，读回 lastVault 并且树要能出来。
{
  const { child, cdp } = await boot()
  const back = await evaluate(cdp, `localStorage.getItem('knowisle.lastVault')`)
  // 必须轮询。`waitForDocument` 在 `location.href` 一变成 tauri.localhost 就返回，
  // 那一刻 React 还没挂载、`vaultScan` 更没回来，直接读行数永远是 0——曾据此
  // 误判成「预热没落盘」，其实 localStorage 早就写进去了。
  let rows = 0
  const rowsDeadline = Date.now() + 15000
  while (Date.now() < rowsDeadline) {
    rows = (await evaluate(cdp, `document.querySelectorAll('.tree-row').length`)) ?? 0
    if (rows > 0) break
    await sleep(20)
  }
  cdp.close()
  killTree(child.pid)
  await waitPortClosed()
  await sleep(400)
  if (back !== VAULT || rows === 0) {
    throw new Error(
      `预热没落盘：lastVault=${JSON.stringify(back)}，树行数=${rows}。` +
        'WM_CLOSE 没来得及让 WebView2 收尾，这轮数字不可信。',
    )
  }
  console.log(`  预热校验：lastVault 读回正常，树 ${rows} 行`)
}

/* ───────────────────── 冷启动 ───────────────────── */

const cold = []
for (let i = 1; i <= ROUNDS; i += 1) {
  const t0 = process.hrtime.bigint()
  const child = launch()
  let elapsed = null
  let diag = null
  const wsUrl = await waitForPage(Date.now() + 20000)
  if (wsUrl !== null) {
    const cdp = await openWs(wsUrl)
    await cdp.send('Runtime.enable')
    const deadline = Date.now() + 20000
    while (Date.now() < deadline) {
      let probe = null
      try {
        probe = await evaluate(cdp, PROBE)
      } catch {
        // 执行上下文还没建好
      }
      if (probe !== null && probe.ready && probe.topbar && probe.rows > 0) {
        elapsed = ms(t0)
        break
      }
      await sleep(5)
    }
    if (elapsed === null) diag = await evaluate(cdp, DIAG).catch((e) => String(e))
    cdp.close()
  }
  killTree(child.pid)
  await waitPortClosed()

  if (elapsed === null) {
    throw new Error(`第 ${i} 轮：20s 内界面没就绪；现场 ${JSON.stringify(diag)}`)
  }
  cold.push(Math.round(elapsed))
  console.log(`  轮 ${i}: ${cold.at(-1)} ms`)
  await sleep(COOLDOWN_MS)
}

const sorted = [...cold].sort((a, b) => a - b)
const pick = (q) => sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))]
const p50 = pick(0.5)
const p95 = pick(0.95)

/* ───────────────────── 空载 RSS + T_edit ───────────────────── */

/** 该进程子树的工作集之和与私有字节之和（字节）。rootPid 是 spawn 拿到的 knowisle.exe。 */
const treeRss = (rootPid) => {
  const raw = psRun(`
$ProgressPreference = 'SilentlyContinue'
Get-CimInstance Win32_Process |
  Select-Object ProcessId, ParentProcessId, Name, WorkingSetSize, PrivatePageCount |
  ConvertTo-Json -Compress
`)
  const all = JSON.parse(raw)
  const byParent = new Map()
  const byId = new Map()
  for (const p of all) {
    const pid = Number(p.ProcessId)
    byId.set(pid, p)
    const key = Number(p.ParentProcessId)
    if (!byParent.has(key)) byParent.set(key, [])
    byParent.get(key).push(pid)
  }
  let total = 0
  let self = 0
  let selfPrivate = 0
  let count = 0
  const stack = [rootPid]
  while (stack.length > 0) {
    const pid = stack.pop()
    const node = byId.get(pid)
    if (node) {
      const ws = Number(node.WorkingSetSize)
      total += ws
      if (pid === rootPid) {
        self = ws
        selfPrivate = Number(node.PrivatePageCount)
      }
      count += 1
    }
    for (const child of byParent.get(pid) ?? []) stack.push(child)
  }
  return { total, self, selfPrivate, children: total - self, count }
}

killAll()
await waitPortClosed()
await sleep(400)
let rss = null
let tEdit = null
let tEditFocused = null
{
  const { child, cdp } = await boot()
  const deadline = Date.now() + 20000
  while (Date.now() < deadline) {
    const probe = await evaluate(cdp, PROBE).catch(() => null)
    if (probe?.ready && probe.rows > 0) break
    await sleep(20)
  }

  // 空载定义：打开库、不打开笔记、窗口在前台、静置 IDLE_SECONDS。
  await cdp.send('Page.bringToFront').catch(() => {})
  await sleep(IDLE_SECONDS * 1000)
  const samples = []
  for (let i = 0; i < 5; i += 1) {
    samples.push(treeRss(child.pid))
    await sleep(2000)
  }
  const med = (arr) => [...arr].sort((a, b) => a - b)[Math.floor(arr.length / 2)]
  rss = {
    totalMB: +(med(samples.map((s) => s.total)) / 1048576).toFixed(1),
    selfMB: +(med(samples.map((s) => s.self)) / 1048576).toFixed(1),
    selfPrivateMB: +(med(samples.map((s) => s.selfPrivate)) / 1048576).toFixed(1),
    childrenMB: +(med(samples.map((s) => s.children)) / 1048576).toFixed(1),
    processCount: samples[0].count,
  }

  // T_edit：从「点第一篇」到「编辑器出现且拿到焦点」。这不是启动开销，
  // 单独报，用来区分「启动慢」和「打开笔记慢」。
  //
  // 必须选 `.tree-row--file`：树是「目录在前」排序，第一个 `.tree-row` 是文件夹，
  // 点它只会展开，永远不会出现 `.cm-content`——曾据此误以为点击没生效。
  const rect = await evaluate(
    cdp,
    `(() => {
      const el = document.querySelector('.tree-row--file')
      if (!el) return null
      const r = el.getBoundingClientRect()
      return { x: r.left + 40, y: r.top + r.height / 2 }
    })()`,
  )
  if (rect !== null) {
    const t0 = process.hrtime.bigint()
    await cdp.send('Input.dispatchMouseEvent', {
      type: 'mousePressed',
      x: rect.x,
      y: rect.y,
      button: 'left',
      buttons: 1,
      clickCount: 1,
    })
    await cdp.send('Input.dispatchMouseEvent', {
      type: 'mouseReleased',
      x: rect.x,
      y: rect.y,
      button: 'left',
      buttons: 0,
      clickCount: 1,
    })
    const editDeadline = Date.now() + 10000
    while (Date.now() < editDeadline) {
      const v = await evaluate(
        cdp,
        `(() => ({
          cm: !!document.querySelector('.cm-content'),
          focused: document.activeElement?.classList?.contains('cm-content') === true,
        }))()`,
      ).catch(() => null)
      // 只把「编辑器出现」当作计时终点，焦点单独记。
      // 这里点击发生在 25 秒静置之后，窗口可能已经不在前台，
      // `document.activeElement` 于是不一定是 `.cm-content`——
      // 把它并进判据会让这个指标在长测里稳定地变成 null（实测踩过）。
      if (v?.cm) {
        tEdit = Math.round(ms(t0))
        tEditFocused = v.focused
        break
      }
      await sleep(5)
    }
  }

  cdp.close()
  killTree(child.pid)
  await waitPortClosed()
}

/* ───────────────────── 汇总 ───────────────────── */

const report = {
  app: path.basename(APP),
  rounds: cold,
  // K2：spawn → 侧栏树渲染完毕。含约 1.7s 的 Tauri + WebView2 固定成本。
  k2ColdStartP50Ms: p50,
  k2ColdStartP95Ms: p95,
  k2ColdStartMinMs: sorted[0],
  k2ColdStartMaxMs: sorted.at(-1),
  k2BudgetMs: LIMIT_MS,
  idleRss: rss,
  selfPrivateBudgetMB: LIMIT_SELF_MB,
  openNoteToEditorMs: tEdit,
  openNoteGotFocus: tEditFocused,
}

console.log(JSON.stringify(report, null, 2))

const k2Ok = p95 <= LIMIT_MS
const rssOk = rss !== null && rss.selfPrivateMB <= LIMIT_SELF_MB
if (k2Ok && rssOk) {
  console.log('PERF PASS')
} else {
  if (!k2Ok) console.log(`K2 超预算：p95 ${p95}ms > ${LIMIT_MS}ms`)
  if (!rssOk) console.log(`空载内存超预算：knowisle.exe 私有 ${rss?.selfPrivateMB}MB > ${LIMIT_SELF_MB}MB`)
  console.log('PERF FAIL')
}
process.exit(k2Ok && rssOk ? 0 : 1)
