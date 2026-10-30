/**
 * 前端访问 Rust 的**唯一入口**。
 *
 * 组件只准从这里 import。`@tauri-apps/*` 出现在 `api/` 之外，就是边界漏了。
 *
 * # 契约从哪来
 *
 * `bindings.ts` 由 Rust 单向生成（`pnpm bindings:generate`），**禁止手改**，
 * CI 会跑 `pnpm bindings:check` 校验 drift。本文件是它上面唯一的一层薄壳：
 * 把 `commands.vaultScan` 摊平成命名导出，组件里写的是 `vaultScan(root)`。
 *
 * 这层壳不是仪式感——它保证"命令要加前置校验 / 打点 / 统一日志"时只有一个
 * 改动点，而不是在十几个组件里各改一遍。
 */

import { commands } from './bindings'

export { commands }

export type {
  AppInfo,
  EncodingEntry,
  EncodingStats,
  Fallback,
  FileNode,
  IndexSkipEntry,
  IndexSkipReason,
  IndexSkipStats,
  IndexState,
  KiError,
  NodeKind,
  NoteContent,
  PurgeResult,
  RestoreResult,
  SaveResult,
  SaveState,
  ScanResult,
  ScanSkipEntry,
  ScanSkipReason,
  SearchHit,
  SearchResult,
  TrashCursor,
  TranscodeBatch,
  TranscodeFailure,
  TranscodeProgress,
  TrashEntry,
  TrashPage,
} from './bindings'

/**
 * 递归扫描 Vault，返回 POSIX 相对路径的树。
 *
 * **不等索引**：返回时索引还在后台建（10k 库首次打开要 10 秒到两分钟）。
 * 之后每次 {@link vaultSearch} 都带 `indexState` 回来，界面据此说
 * 「索引正在建立（N/M），以下结果可能不全」。
 *
 * 不要用 `setInterval` 去轮询状态：状态跟着搜索走就够了，用户没在搜的时候
 * 界面不需要为它转圈。
 */
export const vaultScan = commands.vaultScan

/**
 * 全文搜索。
 *
 * # 语义
 *
 * **命中内容仍然是字面子串**（ASCII 大小写折叠之后），不模糊、不支持正则。
 * 查询按空白拆成多个词时取 **AND**（每个词都得出现在同一行里）。
 * 变的是"怎么找到候选"：M4 之后候选由 SQLite FTS5 选出（英文走词首前缀，
 * 中文走 bigram），再由同一个定位器逐行做字面判定。所以检索精度与之前相同。
 *
 * 两处已知差异，都写在 ADR-007 里：
 * - **ASCII 词中子串不保证**：`us` 搜不到正文里的 `Rust`（词首前缀 `ru` 可以）。
 * - `truncated` **只表示** `total` 是下界（候选池没数全），UI 必须说「至少 N 条」。
 *   `hits` 只返回前 200 条是另一件事，它永远成立，不代表 `total` 不准。
 *
 * # 参数
 *
 * `refreshPath` 传当前正在编辑的笔记路径：Rust 会把它从磁盘重读一次，
 * 让刚打的字立刻可见。其余文件不重读。
 *
 * 调用方必须**先** flush 当前笔记（`noteEdit` → `noteFlush`），
 * 否则磁盘上还没有刚打的字，搜索自然找不到——那不是搜索的 bug。
 *
 * # 索引从哪来
 *
 * `vaultScan` 之后增量同步（判据 `size + mtime`，没变化就读 0 字节），
 * **在后台跑**。索引库从没同步过时，搜索会自己先扫一遍，所以单独调它
 * 也不会静默返回空——但如果后台正在建，它会让路，返回部分结果 + `building`。
 *
 * `indexState` 是结果的一部分，不是另一份要自己拼时间点的数据：
 * 两次 IPC 之间后台可能刚好写完一批。看到 `building` 就说「结果可能不全」。
 */
export const vaultSearch = commands.vaultSearch

/** 读取单篇笔记。`path` 必须是 `vaultScan` 返回过的 POSIX 相对路径。 */
export const noteRead = commands.noteRead

/**
 * 原子地写入单篇笔记。
 *
 * `content` 是**完整正文**，不是增量；父目录必须已存在（不会代建）。
 * 写入是原子的，崩溃只会看到旧内容或新内容。
 *
 * 这是"无条件覆盖"：不做冲突检查。编辑器请用 {@link noteEdit}。
 */
export const noteSave = commands.noteSave

/**
 * 把一篇非 UTF-8 的笔记**显式地**转存为 UTF-8，返回转存后的正文。
 *
 * 这是「保存写回原编码」那条红线的**唯一例外**，所以调用前必须有用户
 * 明确的动作（入口按钮 + 二次确认）。没有「打开就转」「保存就转」。
 *
 * # 三条硬约束（Rust 侧兑现）
 *
 * 1. **先备份**：原字节逐字节进 `.knowisle/history/<path>/<unix_ms>.md`。
 *    备份写不进去就整个失败——没有备份的转存不可撤销。
 * 2. **原子写**：`tmp → fsync → rename`。
 * 3. **幂等**：磁盘上本来就是 UTF-8 时不写盘、不动 mtime、不留备份。
 *
 * # 参数
 *
 * `expectedMtime` 传**磁盘上当前那份**的 mtime（打开时读到的，或最近一次
 * 成功落盘的）。过期返回 `CONFLICT`，此时磁盘一个字节都没动。
 *
 * # 返回值必须用来替换 `activeNote`
 *
 * `size` 与 `mtime` 都变了。只改本地 state 会让下一次自动保存拿着旧
 * mtime 撞 `CONFLICT`——看起来像「转存之后保存坏了」。同时
 * `decodedFrom` 变成 `null`，顶部那行「已按 GBK 解码」随之消失。
 */
export const noteTranscodeToUtf8 = commands.noteTranscodeToUtf8

/**
 * 新建一篇空笔记，返回它的 Vault 相对路径。
 *
 * `dir` 是 Vault 相对目录（空串表示根目录），必须已存在——不会代建目录。
 * `name` 没有 `.md` / `.markdown` 后缀会自动补 `.md`。
 *
 * 文件名合法性**只在 Rust 侧判定**（`ki_vault::naming`）。前端可以提前给
 * 一句提示，但那只是即时反馈，不是闸门——第二个入口（将来的 `isle` CLI）
 * 走的是同一份 Rust 规则。
 *
 * 撞名返回 `NAME_TAKEN`，原文件一个字节都不动。
 */
export const noteCreate = commands.noteCreate

/**
 * 库级编码统计：**不打开任何一篇**就知道库里有多少篇不是 UTF-8。
 *
 * 数据来自索引库的 `note.src_encoding` 列（索引器读正文时顺手记的），
 * 所以这条命令不扫盘、不读正文。
 *
 * # `truncated` 与 `total` 的分工
 *
 * `total` 永远是**精确**的待转存篇数；`truncated` 只说明 `nonUtf8` 清单被截到
 * 了前 1000 条。别把清单长度当总数。
 *
 * # `indexState` 必须看
 *
 * 索引在后台建，建到一半时 `total` 只是「目前为止数到的」。前端要据此
 * 继续等（`state === 'building'`），而不是把 0 当成「库里没有非 UTF-8」——
 * 那会让入口永远不出现。
 */
export const vaultEncodingStats = commands.vaultEncodingStats

/**
 * 索引侧跳过清单：哪几篇没进索引、各是为什么。
 *
 * # 它与 `vaultEncodingStats` 数的是两批**互斥**的行
 *
 * `vaultEncodingStats` 数 `src_encoding IS NOT NULL`（能被读取、编码不是
 * UTF-8）；这条数 `status = 'skipped'`（**根本没被索引**）。一篇要么成功
 * 索引、要么被跳过，所以同一篇不可能同时出现在两个清单里。
 *
 * # 它与侧栏那个「跳过 N」数的是两件不同的事
 *
 * 侧栏那个来自 `ScanResult.skipped`——**扫描**时跳过的**条目**（目录、软链、
 * FIFO、非 UTF-8 文件名）。这条数的是**索引**时跳过的**笔记**（认不出编码、
 * 超过 2MB、IO 失败）。两个数任何时候不相等也合法，前端必须分开显示、
 * 分开计数，不许相加或取其中一个（ADR-013）。
 *
 * # `indexState` 必须看
 *
 * 索引在后台建，建到一半时 `total` 只是「目前为止数到的」。前端要据此
 * 继续轮询（`state === 'building'`），而不是把 0 当成「没有跳过」——
 * 那会让入口永远不出现。
 */
export const vaultSkippedNotes = commands.vaultSkippedNotes

/**
 * 批量转存：把一批非 UTF-8 的笔记逐篇改写成 UTF-8。
 *
 * 每一篇都走 `note_transcode_to_utf8` 背后那条实现（备份 → 原子写 → 重读），
 * **没有第二份批量实现**。
 *
 * # 三条语义
 *
 * 1. **失败不中断**：单篇失败记进 `failed` 继续跑，结果逐条带路径与原因。
 * 2. **可取消**：`token` 由前端生成（单调递增），取消命令靠它定位批次。
 *    取消后已转好的**不回滚**。
 * 3. **幂等**：已经是 UTF-8 的篇返回成功、不写盘、不留备份。
 *
 * # 进度
 *
 * 每跑完一篇 Rust 侧更新一次真实进度，前端按 `token` 轮询
 * `noteTranscodeProgress`。不许按「发了几个请求」猜。
 */
export const noteTranscodeBatch = commands.noteTranscodeBatch

/** 查一批转存的当前进度。`null` = 这一批已经结束（不是「进度归零」）。 */
export const noteTranscodeProgress = commands.noteTranscodeProgress

/** 请求取消一批转存。粒度是「篇与篇之间」，幂等。 */
export const noteTranscodeCancel = commands.noteTranscodeCancel

/**
 * 重命名一篇笔记（同目录内改名，不移动、不删除），返回新的相对路径。
 *
 * 调用前必须先 `noteEdit` + `noteFlush` 把编辑器 300ms 合并窗口里的字交出去，
 * 但那**不能替代** Rust 侧的 drain：前端 flush 返回时自动保存的 worker
 * 可能还没退出，而它会把内容写到已经不存在的旧路径上（旧文件复活）。
 * 顺序由 `note_rename` 自己在 Rust 侧保证。
 *
 * 目标已存在返回 `NAME_TAKEN`（只改大小写除外——那是同一个文件）。
 */
export const noteRename = commands.noteRename

/**
 * 删除一篇笔记：**移进 `.knowisle/trash/`**，返回它在回收目录里的相对路径。
 *
 * 调用前必须先 `noteEdit` + `noteFlush`（当前笔记时），否则 trash 里会是旧版本。
 * 但那**不能替代** Rust 侧的 drain：前端 flush 返回时自动保存的 worker 可能
 * 还没退出。顺序由 `note_delete` 自己在 Rust 侧保证：
 * drain → 搬进 trash → 写清单 → 清空槽位。
 *
 * 冲突时拒绝删除（`CONFLICT`）：此刻用户还有「覆盖 / 放弃」两个选择，
 * 删掉就只剩一个。
 *
 * # 它现在真的是回收站
 *
 * 删除时会记下原路径，`trashList` 能列出来，`trashRestore` 能搬回原目录。
 * 唯一找不回来的情况是清单损坏——那时仍能恢复，只是落到库根目录
 * （返回值里 `fallback` 会说明是三种原因里的哪一种，UI 必须分开措辞）。
 */
export const noteDelete = commands.noteDelete

/**
 * 列出回收站的**一页**，按删除时间倒序。
 *
 * 每一项带 `entry`（恢复用的磁盘真名）、`name`（展示名）、`origin`
 * （原路径，可能为 `null`）、`deletedAt`、`size`。
 *
 * `origin === null` 是**正常状态**：清单缺失或损坏。UI 必须显示成
 * 「原路径未知」，不能拿 `name` 拼一个看起来像路径的字符串——那是在骗用户。
 *
 * # 三个字段各回答一个问题
 *
 * - `entries`：这一页给了多少条。
 * - `total`：回收目录里**一共**有多少条。分页之后它与 `entries.length`
 *   必然分叉，别拿后者当总数。
 * - `next`：下一页的游标，`null` = 没有下一页。它是服务端算出来的**事实**
 *   （多取一条判出来的），**不要**用 `entries.length < limit` 自己推——
 *   目录被跳过、条目在两次调用之间被恢复，都会让"这页不满"而其实还有下一页。
 *
 * # 参数
 *
 * - `cursor` 首页传 `null`，之后传上一页的 `next`，原样传回，不要自己拼。
 * - `limit` 服务端钳制（`0 → 200`、`> 500 → 500`）。
 *
 * 列表为空不报错（从没删过东西的库就是这样）。
 */
export const trashList = commands.trashList

/**
 * 把回收站里的一条搬回原目录。
 *
 * `name` 传列表里的 `entry`（不是展示名）。
 *
 * # 三个字段各说一件事，别只看其中一个
 *
 * - `path` / `requested`：**事实** / **意图**。降级时两者分叉
 *   （`requested` 是 `a/b/c.md`，`path` 是 `c.md`），所以
 *   `path !== requested` **不代表**改了名。
 * - `fallback`：为什么没回原处。`null` = 原路回去了；
 *   `'recordMissing'` / `'recordCorrupt'` / `'originGone'` 是三种不同的下一步，
 *   措辞必须不同（见 `App.describeRestore`）。
 * - `renamed`：有没有**真的**撞名改名。与 `fallback` **正交**：
 *   `originGone` 落到库根时，根目录也可能已有同名文件。
 * - `indexed`：索引里有没有这一行。`false` 不是失败——文件已经在正确位置，
 *   判成失败只会诱导用户重试（重试会得到 `c 2.md`）。
 *
 * 恢复之后索引由命令自己定点补一行，所以**不重扫也能立刻搜到**。
 * 但树要刷新仍然得重扫（那是另一件事）。
 */
export const trashRestore = commands.trashRestore

/**
 * 清空回收站，返回「删掉了几个、还剩几个」。
 *
 * 不递归删用户自己放进回收目录的子目录（那不是应用造的东西）。
 *
 * **永远成功**（除非回收目录本身读不出来）：部分文件删不掉时其余照删，
 * `failed` 计数，它们仍然躺在回收目录里。前端必须刷新列表并把
 * 「已永久删除 N 篇，M 篇删不掉」说出去——旧实现返回错误，于是面板上
 * 还列着已经被删掉的文件，一个面板两个真相。
 */
export const trashPurge = commands.trashPurge

/**
 * 提交一次编辑，由 Rust 侧的节流器决定何时落盘（1.5 秒）。
 *
 * `baseMtime` 传打开这篇笔记时 `noteRead` 返回的 mtime，
 * 只在切换文档时被采信一次——基线随后由 Rust 随每次成功落盘向前推进。
 *
 * 返回 {@link SaveState}：是否有内容未落盘、最近一次落盘的 mtime、最近一次错误。
 */
export const noteEdit = commands.noteEdit

/** 立刻落盘，不等节流周期。切换笔记、关窗前调用。 */
export const noteFlush = commands.noteFlush

/** 查询保存状态，不触发写入。只在前端 `dirty` 为真时轮询。 */
export const noteStatus = commands.noteStatus

/**
 * 把自动保存的乐观锁基线对齐到磁盘上的新 mtime。**不写盘。**
 *
 * 两个调用点，都是「磁盘变了、但没有一次属于我们的落盘」：
 *
 * - 聚焦校验判出 `adopt-baseline`（外部工具原样重存：内容没变、mtime 变了）；
 * - 用户点了「重新加载」（此时 `dropPending` 为真，槽位里压着的旧内容一并丢弃）。
 *
 * # 为什么必须有一个 IPC
 *
 * {@link noteEdit} 的 `baseMtime` **只在切换文档时被采信一次**（见
 * `ki-ipc/src/autosave.rs` 的 `submit`）。同一篇的后续调用读的是槽位里的基线，
 * 而基线只在成功落盘时前进。前端没有任何参数能让它前进——所以「重新加载」
 * 之后不调这个，下一次保存必然撞一个假的 `CONFLICT`。
 *
 * # 返回 `false` 不是错误
 *
 * 槽位里不是这一篇（不需要补救），或者 `dropPending = false` 而槽位里还有
 * 真正不同的待写内容（拒绝推进——否则那份内容会带着新基线盖掉别人的版本）。
 */
export const noteRebase = commands.noteRebase

/**
 * 健康检查：确认 Rust 侧真的起来了。
 *
 * 启动时先 ping 一次。如果这一句失败，后面所有按钮都会失败，
 * 报错信息会误导排查方向。
 */
export const appPing = commands.appPing

export { describeError, isKiError, isMissingFile } from './errors'
export { pickFolder } from './dialog'
