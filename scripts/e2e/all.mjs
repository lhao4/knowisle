// 顺序跑完全部端到端验收：`pnpm e2e:all`。
//
// # 为什么不是 `a && b && c`
//
// 两条理由：
//
// 1. **每一步之间要重启应用**。调试槽位是进程级静态，探针靠
//    `Page.addScriptToEvaluateOnNewDocument` 注入——同一个进程里跑第二个脚本，
//    探针会重复注入，症状是"某个计数器莫名翻倍"（B-16）。所以每一步都带
//    `--restart`：脚本自己 `taskkill` 再 `spawn`，这才是真的换了进程。
// 2. **一条红了不要遮住后面七条**。`&&` 会在第一处失败就停，而"哪几条红"
//    本身就是验收要的信息。这里跑完全部再汇总，退出码只在最后决定。
//
// 退出码：0 全绿 / 1 有失败 / 2 跑完了但有跳过（B-19：跳过不能当绿）。
import { spawn } from 'node:child_process'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))

/**
 * 顺序有依赖，不能随便调：
 * - `trash-page` 排最后：它每轮都要 `cargo test` 重造 10k 条夹具，是所有步骤里
 *   最慢的一步，而且它自己结尾会 `killApp()`（清空 1 万条之后那个进程里的
 *   索引缓存已经没有意义）。放最后，前面六步的快速反馈不必等它。
 * - `encoding` 插在 `theme` 之后、`trash-page` 之前：它要一个干净的夹具库，
 *   但**不能**跟在 `trash-page` 后面——那一步结尾会 `killApp()`，收尾的
 *   kill 会让后面脚本的 `--restart` 撞上一个正在退出的进程。
 * - `external` 紧随 `encoding`：同一条约束（要干净夹具库、不能排在
 *   `trash-page` 之后），且它自己也从 Node 侧改磁盘，与编码那条互不影响。
 * - 其余各自一个夹具库，互不干扰。
 */
const STEPS = [
  ['e2e:trash', 'cdp-trash.mjs', '回收站：删除 / 列表 / 恢复 / 清空'],
  ['e2e:trash-restore', 'cdp-trash-restore.mjs', '回收站：恢复的降级与撞名'],
  ['e2e:tree', 'cdp-tree.mjs', '文件树：删除 + 新建 / 重命名'],
  ['e2e:outline', 'cdp-outline.mjs', '大纲：跟随光标 / 折叠 / 计数'],
  ['e2e:keys', 'cdp-keys.mjs', '快捷键：新建 / 保存 / 加粗 / 斜体'],
  ['e2e:theme', 'cdp-theme.mjs', '主题：三态切换 / 对比度 / 闪烁'],
  ['e2e:encoding', 'cdp-encoding.mjs', '编码：转存为 UTF-8'],
  ['e2e:encoding-batch', 'cdp-encoding-batch.mjs', '编码：库级清单 + 批量转存'],
  ['e2e:external', 'cdp-external-change.mjs', '外部改动：聚焦校验 / 重新加载 / 保留我的'],
  ['e2e:trash-page', 'cdp-trash-page.mjs', '回收站：10k 条分页'],
]

const run = (file) =>
  new Promise((resolve) => {
    const child = spawn(process.execPath, [path.join(HERE, file), '--restart'], {
      stdio: 'inherit',
      env: process.env,
    })
    child.on('close', (code) => resolve(code ?? 1))
  })

const results = []
const startedAll = Date.now()

for (const [index, [name, file, what]] of STEPS.entries()) {
  if (index > 0) {
    console.log('\n' + '─'.repeat(72))
    console.log('重启应用：下一个脚本要求一个新进程（B-16 调试槽位是进程级静态）')
    console.log('─'.repeat(72))
  }
  console.log(`\n━━━ [${index + 1}/${STEPS.length}] ${name} · ${what} ━━━\n`)

  const started = Date.now()
  const code = await run(file)
  results.push({ name, code, ms: Date.now() - started })
  console.log(`\n━━━ ${name} 退出码 ${code} · ${Math.round((Date.now() - started) / 1000)}s ━━━`)
}

const failed = results.filter((r) => r.code === 1 || r.code > 2)
const skipped = results.filter((r) => r.code === 2)

console.log('\n' + '═'.repeat(72))
for (const r of results) {
  const mark = r.code === 0 ? 'OK  ' : r.code === 2 ? 'SKIP' : 'FAIL'
  console.log(`${mark}  ${r.name.padEnd(18)} ${String(Math.round(r.ms / 1000)).padStart(4)}s`)
}
console.log(`总计 ${Math.round((Date.now() - startedAll) / 1000)}s`)
if (failed.length > 0) console.log(`✗ ${failed.map((r) => r.name).join(', ')} 有失败`)
if (skipped.length > 0) console.log(`⚠ ${skipped.map((r) => r.name).join(', ')} 有跳过，本次验收不完整`)
console.log('═'.repeat(72))

process.exit(failed.length > 0 ? 1 : skipped.length > 0 ? 2 : 0)
