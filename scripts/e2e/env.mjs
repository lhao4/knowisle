// 端到端脚本的公共路径与环境。
//
// 三条规矩（都是踩过的）：
//
// 1. **不许出现本机绝对路径**。脚本入库之后别人也要能跑，而写死的盘符路径
//    只在这台机器上成立。仓库根从 `import.meta.url` 反算。
// 2. **夹具放系统临时目录**，不放仓库的 `.tmp/`。本机实测：`.tmp` 里删一个
//    文件 390ms，`%TEMP%` 里 1.8ms——1000 篇的夹具差出几十秒（TOOLING）。
// 3. **端口可以用环境变量覆盖**。同时开两个调试实例时，9222 会被先起的那个
//    占住，脚本连到错误的 target 上，症状是"页面里什么都没有"。
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/** 仓库根（`scripts/e2e/` 往上两层）。 */
export const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')

/** 快速打开 / 全文搜索 / 文件树共用的夹具库：1000 篇 + 2 个坏文件。 */
export const QUICK_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-vault-quick')

/** 后台索引状态用例的夹具库：更大，好让「正在建」这段窗口足够宽。 */
export const INDEX_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-vault-index')

/** 回收站三件各自的夹具库。分开是刻意的：三个脚本各自要求「应用刚重启」，
 *  共用一个库会让上一轮恢复出来的文件改变下一轮的撞名落点（B-32）。 */
export const TRASH_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-trash')
export const TRASH_RESTORE_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-trash-restore')
/** 10k 条分页夹具。构造与清理都在 Rust 侧（node 的 rmSync 删不动两万个文件）。 */
export const TRASH_PAGE_VAULT = path.join(os.tmpdir(), 'ki-e2e-trash-page')

/** 快捷键 / 大纲 / 主题各自的夹具库。 */
export const KEYS_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-keys')
export const OUTLINE_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-outline')
export const THEME_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-theme')

/**
 * 编码转存（B-38）的夹具库。
 *
 * 单独一个库而不是塞进 theme：那个脚本会改全局主题偏好、还会往夹具里写字，
 * 而这里的判据全在**磁盘字节**上——共用一个库会让「转存前是什么字节」
 * 依赖上一个脚本跑没跑过（B-32）。
 */
export const ENCODING_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-encoding')

/**
 * 库级「N 篇不是 UTF-8」+ 批量转存（B-39 第十一刀）的夹具库。
 *
 * 单独一个库，理由与上面两条相同：这个脚本会造 1000+ 篇夹具、把其中一批
 * 写成 GBK、还会在中途**从 Node 侧删文件**（造「列出来之后磁盘变了」）。
 * 与 `ENCODING_VAULT` 共用会让「转存前磁盘上是什么字节」依赖上一个脚本
 * 跑没跑过（B-32）。规模也必须独立：980 + 20 是「不打开任何一篇就数出 20」
 * 那条判据的口径，别的脚本改不动它。
 */
export const ENCODING_BATCH_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-encoding-batch')

/**
 * 外部改动（B-09 第十刀）的夹具库。
 *
 * 单独一个库，理由与 `ENCODING_VAULT` 相同：这个脚本会**从 Node 侧直接改磁盘**
 * （写正文、写 GBK 字节、删文件），而它的判据全是「编辑器里的字 vs 磁盘上的
 * 字节」。与别的脚本共用库，会让「打开时磁盘上是什么」依赖上一个脚本跑没跑过
 * （B-32）。
 */
export const EXTERNAL_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-external')

/**
 * 跳过清单（B-04 第十二刀）的夹具库。
 *
 * 单独一个库，理由与上面几条相同：这个脚本要造**无读权限的目录**（Windows
 * 走 `icacls`、Unix 走 `chmod 000`）、**非 UTF-8 文件名**与 **FIFO**（仅 Unix），
 * 而且会在中途**从 Node 侧删文件**造「删掉那篇之后清单少一条」。
 * 与别的脚本共用库会让「起点是什么」依赖上一个脚本跑没跑过（B-32）。
 *
 * 权限类夹具必须能被还原：`resetVault` 的 `sweepNotes` 递归删 `.md`，遇到
 * 一个 000 的目录会直接抛 EACCES——**所以每次重建之前先解锁**。
 */
export const SKIP_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-skip-list')

/**
 * 忽略规则（B-05 第十三刀）的夹具库。
 *
 * 单独一个库，理由与上面几条相同：这个脚本会往 `.knowisle/config.toml` 里
 * **写配置文件**，而 `.knowisle` 在 `sweepNotes` 里是保留的——跨脚本共用库
 * 会让下一个脚本莫名其妙地少一片目录（§6 的已知坑）。脚本自己开头与结尾
 * 各删一次配置，但库仍然要独立。
 */
export const IGNORE_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-ignore-rules')

/**
 * 「重新扫描」（B-09 ①，ADR-017）的夹具库。
 *
 * 单独一个库，理由与上面几条相同，但这里更硬：脚本会**从 Node 侧**
 * 新建目录 / 删当前笔记 / 改名当前笔记 / 把库目录整个改名，还会往回收目录里
 * 直接造 250 条记录（判据 5 要翻到第 2 页）。与任何别的脚本共用库，都会让
 * 「重扫前磁盘上是什么」依赖上一个脚本跑没跑过（B-32）。
 *
 * 规模也是判据的一部分：`bulk/` 2000 篇只为一个目的——让重扫之后的索引同步
 * 窗口足够宽，宽到能在里面打开搜索面板（判据 7）。树里它是折叠的（> 8 个子项），
 * 不占渲染。
 */
export const RESCAN_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-rescan')

/**
 * 「树自己刷新」（B-09 ②，ADR-018）的夹具库。
 *
 * 单独一个库，理由与上面几条相同，而且这里更硬：脚本会**从 Node 侧**
 * 在子目录里新建 / 删除 / 改名笔记，还会反复「切走切回」——判据全是
 * 「切回来之后树里有什么」，而树里有什么取决于磁盘起点。与 `RESCAN_VAULT`
 * 共用会让本脚本的起点被上一个脚本（它会删 `notes/beta.md`、改名
 * `notes/gamma.md`）改掉（B-32）。
 *
 * 规模刻意小（30 篇 / 4 个目录）：探针每次聚焦要 readdir 一遍全库，
 * 夹具越大，10 次「切走切回」越慢，而判据与规模无关。
 */
export const PROBE_VAULT = path.join(os.tmpdir(), 'knowisle-e2e-probe')

/** 脚本产物（验收报告 JSON、截图）。**只写不读**：脚本不许依赖这里的残留。 */
export const OUT_DIR = path.join(REPO, '.tmp')

// 全新 clone 里没有 `.tmp/`（它被 `.gitignore` 忽略，也不会被 `git clone` 带出来）。
// 不建的话，脚本会**跑完全部判据之后**崩在写报告那一行——ENOENT，看起来像
// 判据挂了，其实全绿。这条是「新机器模拟」跑出来的：`git clone` + `pnpm install`
// 之后直接跑，六条判据全过，收尾抛 ENOENT。
fs.mkdirSync(OUT_DIR, { recursive: true })

/** WebView2 的调试端口。 */
export const CDP_PORT = Number(process.env.KI_CDP_PORT ?? 9222)

/** 应用 dev 页面的来源。连 target 时必须认它，不能只认 `type === 'page'`。 */
export const APP_ORIGIN = 'http://127.0.0.1:1420'

/** debug 产物。`tauri dev` / `cargo build -p knowisle` 都落在这里。 */
export const APP_EXE = path.join(
  REPO,
  'target',
  'debug',
  process.platform === 'win32' ? 'knowisle.exe' : 'knowisle',
)

/**
 * 是否由脚本自己杀掉并重启应用。
 *
 * 默认**不**重启：`pnpm e2e:trash` 直接连一个已经开着的应用，连不上就显式失败
 * （B-19：前置缺失必须响，不能空跑出一片绿）。`e2e:all` 传 `--restart`，
 * 每一步都换一个新进程——调试槽位是进程级静态（B-16），同一进程里跑第二个
 * 脚本会让 `addScriptToEvaluateOnNewDocument` 的探针重复注入。
 */
export const RESTART =
  process.argv.includes('--restart') || process.env.KI_E2E_RESTART === '1'

/** 一次 CDP 往返之后给 React 的默认喘息时间。 */
export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))
