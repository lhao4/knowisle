// 端到端脚本的公共路径与环境。
//
// 三条规矩（都是踩过的）：
//
// 1. **不许出现本机绝对路径**。脚本入库之后别人也要能跑，而 `E:\MyProject\...`
//    只在这台机器上成立。仓库根从 `import.meta.url` 反算。
// 2. **夹具放系统临时目录**，不放仓库的 `.tmp/`。本机实测：`.tmp` 里删一个
//    文件 390ms，`%TEMP%` 里 1.8ms——1000 篇的夹具差出几十秒（TOOLING）。
// 3. **端口可以用环境变量覆盖**。同时开两个调试实例时，9222 会被先起的那个
//    占住，脚本连到错误的 target 上，症状是"页面里什么都没有"。
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/** 仓库根（`scripts/e2e/` 往上两层）。 */
export const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')

/** 快速打开 / 全文搜索共用的夹具库：1000 篇 + 2 个坏文件。 */
export const QUICK_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-vault-quick')

/** 后台索引状态用例的夹具库：更大，好让「正在建」这段窗口足够宽。 */
export const INDEX_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-vault-index')

/** 脚本产物（验收报告 JSON）。 */
export const OUT_DIR = path.join(REPO, '.tmp')

/** WebView2 的调试端口。 */
export const CDP_PORT = Number(process.env.KI_CDP_PORT ?? 9222)

/** 应用 dev 页面的来源。连 target 时必须认它，不能只认 `type === 'page'`。 */
export const APP_ORIGIN = 'http://127.0.0.1:1420'

/** 一次 CDP 往返之后给 React 的默认喘息时间。 */
export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
