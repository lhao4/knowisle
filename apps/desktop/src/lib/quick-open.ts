/**
 * 快速打开（`Cmd/Ctrl+P`）的数据层：树 → 扁平列表，查询 → 排序结果。
 *
 * # 为什么扁平化放在「扫描完成时」而不是「打开面板时」
 *
 * 面板每次打开、每次按键都要一个候选列表。如果那时才遍历树，1000 篇的库
 * 就是每按键一次 1000 个节点的递归 + 1000 次字符串拼接——它比匹配本身还贵，
 * 而且结果每次都一样。所以树进内存的那一刻就把 `QuickItem[]` 算好，
 * 面板只做「遍历数组 + 打分」，O(1) 次树操作。
 *
 * # 为什么不用现成的模糊匹配库
 *
 * 需求只有三条打分规则（连续 > 分散、词首 > 中间、大小写一致加分），
 * 任何通用库都要为这三条引入几十 KB 和一套自己的 API。这里的实现是
 * 一个 60 行的动态规划，规则写在常量里，改权重不用读别人的文档。
 *
 * # 为什么是动态规划而不是贪心
 *
 * 贪心从左往右取第一个能匹配的位置。查询 `ab` 在 `a-x-ab` 里会先咬住
 * 开头那个 `a`，于是拿到「分散」的分，而正确答案是结尾那个连续 `ab`。
 * 打分规则第一条就是「连续 > 分散」，贪心会在最该赢的地方输掉。
 * DP 求的是全局最优，代价是 O(查询长度 × 路径长度)。
 */

import type { FileNode } from '../api'

/** 一个候选：文件名（去扩展名）+ 所在目录 + 打开时用的完整路径。 */
export interface QuickItem {
  /** Vault 相对路径（POSIX），原样传给 `noteRead`。 */
  path: string
  /** 文件名，**去掉 `.md`**。展示用。 */
  name: string
  /** 所在目录（POSIX，根目录为空串）。展示用，也参与「文件名优先」打分。 */
  dir: string
}

/** 列表最多显示多少项。空查询是「前 50 篇」，非空查询是「前 50 个命中」。 */
export const QUICK_LIMIT = 50

/* ───────────────────────── 扁平化 ───────────────────────── */

/**
 * 把扫描出来的树按**先序**压成一维数组：目录展开在原地，文件按树里的顺序
 * 依次出现。于是返回值的前 N 项就是「侧栏从上往下数前 N 篇」。
 *
 * 用显式栈而不是递归：扫描器允许很深的目录（实测 136 层），递归版本在
 * 更深的库上会爆栈，而这里没有任何理由替用户设一个深度上限。
 *
 * # 这里不排序
 *
 * 顺序由扫描器给（`ki-vault/src/sort.rs`：目录在前 → 拼音 → 数字感知）。
 * 前端再排一遍就是第二份顺序——两份顺序在小库上看着一样，改任何一边都会
 * 分叉。这个函数曾经按 `path` 的 UTF-16 码位排，注释里写着「拼音排序是
 * B-07 的事，改的时候两边一起改」；ADR-016 兑现了那句：扫描器换成拼音序，
 * 这里同时把排序删掉。
 *
 * 也因此不能改用 `localeCompare`：它依赖 WebView 自带的 ICU 与系统语言，
 * 同一个库在两台机器上会给出不同的「前 50 篇」。排序数据必须锁在
 * `Cargo.lock` 里（ICU4X 的 `compiled_data`），不能来自运行环境。
 */
export function flattenVault(tree: FileNode): QuickItem[] {
  const items: QuickItem[] = []
  const stack: FileNode[] = [tree]

  while (stack.length > 0) {
    const node = stack.pop()
    if (node === undefined) break

    if (node.kind === 'file') {
      const ext = mdSuffix(node.name)
      items.push({
        path: node.path,
        name: ext === 0 ? node.name : node.name.slice(0, node.name.length - ext),
        // 父目录 = path 去掉最后一段。根目录下的文件两者相等，此时为空串。
        dir:
          node.path.length > node.name.length
            ? node.path.slice(0, node.path.length - node.name.length - 1)
            : '',
      })
      continue
    }

    const children = node.children
    if (children == null) continue
    // 逆序入栈，出栈顺序才与树里的原始顺序一致。
    for (let i = children.length - 1; i >= 0; i -= 1) stack.push(children[i])
  }

  return items
}

/* ───────────────────────── 打分 ───────────────────────── */

/** 每命中一个查询字符。 */
const PER_CHAR = 1
/** 命中在词首：路径开头、`/` 之后、`-_. ` 之后、camelCase 的大写字母。 */
const AT_WORD_START = 12
/** 命中落在文件名（最后一段）里。 */
const IN_NAME = 4
/** 与前一个命中相邻。 */
const CONSECUTIVE = 8
/** 大小写与查询完全一致（仅 ASCII 字母）。 */
const CASE_EXACT = 2
/** 每跳过一个字符扣的分。 */
const GAP = 1

/** 不可达标记。取 `-2^30`：累加几百个分数也离 Int32 下界很远。 */
const NEG = -1 << 30

/**
 * 复用的打分缓冲。
 *
 * 1000 篇的库，每次按键要给 1000 个候选各算一遍。若每个候选都新建两个
 * `Int32Array`，就是每按键 2000 次分配——GC 压力比计算本身大一个量级。
 * `searchQuick` 是同步、单线程、无递归的，所以这块缓冲不需要保护。
 */
let prevBuf = new Int32Array(0)
let curBuf = new Int32Array(0)

function ensureBuffers(n: number): void {
  if (prevBuf.length >= n) return
  prevBuf = new Int32Array(n)
  curBuf = new Int32Array(n)
}

/** ASCII 小写折叠。非 ASCII（中文等）原样返回，逐码位比较。 */
function fold(code: number): number {
  return code >= 65 && code <= 90 ? code + 32 : code
}

/** `.md` 后缀的长度；不是 `.md` 结尾时为 0。大小写不敏感。 */
function mdSuffix(name: string): number {
  const n = name.length
  if (n < 4) return 0
  if (name.charCodeAt(n - 3) !== 46) return 0 // '.'
  if ((name.charCodeAt(n - 2) | 32) !== 109) return 0 // 'm'
  if ((name.charCodeAt(n - 1) | 32) !== 100) return 0 // 'd'
  return 3
}

function isLower(code: number): boolean {
  return code >= 97 && code <= 122
}

function isUpper(code: number): boolean {
  return code >= 65 && code <= 90
}

/**
 * 大小写是否**完全一致**。
 *
 * 只对 ASCII 字母给分：`/`、中文这些没有「大小写」可言，比较相等是必然的，
 * 给分等于给所有候选加同一个常数。
 *
 * 注意这里不能写成「小写也算一致」——那样查询 `API` 会同时给
 * `API-Design` 与 `api-design` 加分，验收 3 的「大小写」一组就永远分不开。
 */
function sameCase(a: number, b: number): boolean {
  return a === b && (isUpper(a) || isLower(a))
}

/**
 * `j` 这个位置是不是「词首」。
 *
 * 除了路径开头与分隔符之后，还认两种边界：
 * - `-` `_` `.` 空格之后（`API-Design` 的 `D`、`v1.2` 的 `2`）
 * - camelCase 的上升沿（`kiStore` 的 `S`）——用户输入 `ks` 时，
 *   `S` 该按词首计分，而不是按「词中间」。
 */
function isWordStart(s: string, j: number): boolean {
  if (j === 0) return true
  const prev = s.charCodeAt(j - 1)
  if (prev === 47 || prev === 92 || prev === 45 || prev === 95 || prev === 46 || prev === 32) {
    return true
  }
  return isLower(prev) && isUpper(s.charCodeAt(j))
}

/**
 * 给一个候选打分；不构成子序列匹配时返回 `null`。
 *
 * `dp[j]` = 「查询的前 `i` 个字符全部命中，且第 `i` 个字符落在 `j`」的最高分。
 * 转移只有两种来源：上一个字符在 `j-1`（连续），或在 `j-2` 及更早（有间隔）。
 * 后者用一个滚动最大值维护，于是整行是 O(路径长度)，总复杂度 O(m·n)。
 */
function scoreItem(item: QuickItem, query: string): number | null {
  const path = item.path
  // 匹配窗口跳过末尾的 `.md`：否则输入 `md` 会命中整个库。
  const end = path.length - mdSuffix(path)
  const m = query.length
  if (m > end) return null

  ensureBuffers(end)
  let prev = prevBuf
  let cur = curBuf

  // 文件名在匹配窗口里的起点。`dir` 为空时文件名就是整个路径。
  const nameStart = item.dir.length === 0 ? 0 : item.dir.length + 1

  for (let i = 0; i < m; i += 1) {
    const qc = query.charCodeAt(i)
    const qf = fold(qc)
    let runMax = NEG
    let matched = false

    for (let j = 0; j < end; j += 1) {
      cur[j] = NEG

      // 把 k = j-2 纳入「上一个命中不与 j 相邻」的候选池。
      // 滚动量是 `dp[k] + GAP*k`，于是 `max_k dp[k] - GAP*(j-1-k)`
      // 可以写成 `runMax - GAP*(j-1)`，不用回扫。
      if (i > 0 && j >= 2) {
        const p = prev[j - 2]
        if (p > NEG) {
          const v = p + GAP * (j - 2)
          if (v > runMax) runMax = v
        }
      }

      const c = path.charCodeAt(j)
      if (fold(c) !== qf) continue

      let base = PER_CHAR
      if (sameCase(c, qc)) base += CASE_EXACT
      if (j >= nameStart) base += IN_NAME
      if (isWordStart(path, j)) base += AT_WORD_START

      let best = NEG
      if (i === 0) {
        best = base
      } else {
        if (runMax > NEG) {
          const cand = runMax - GAP * (j - 1) + base
          if (cand > best) best = cand
        }
        if (j >= 1) {
          const p = prev[j - 1]
          if (p > NEG) {
            const cand = p + CONSECUTIVE + base
            if (cand > best) best = cand
          }
        }
      }

      if (best > NEG) {
        cur[j] = best
        matched = true
      }
    }

    // 这一行一个位置都对不上 → 查询不可能是子序列，后面的行不用算了。
    if (!matched) return null

    const swap = prev
    prev = cur
    cur = swap
  }

  let best = NEG
  for (let j = 0; j < end; j += 1) {
    if (prev[j] > best) best = prev[j]
  }
  return best > NEG ? best : null
}

/**
 * 查询 → 最多 `limit` 个候选。
 *
 * 空查询返回**树序的前 `limit` 篇**（与侧栏同序，扁平化时已经是这个顺序）。
 * 不做「最近打开」：那是第二份状态，要持久化、要失效、要跟删除对账，
 * 换来的是列表顶部几条更顺眼——不值。界面上会明说这一点。
 */
export function searchQuick(
  items: QuickItem[],
  query: string,
  limit: number = QUICK_LIMIT,
): QuickItem[] {
  if (query.length === 0) {
    return items.length > limit ? items.slice(0, limit) : items
  }

  const hits: { item: QuickItem; score: number }[] = []
  for (const item of items) {
    const score = scoreItem(item, query)
    if (score !== null) hits.push({ item, score })
  }

  hits.sort(compareHits)

  const n = hits.length < limit ? hits.length : limit
  const out: QuickItem[] = new Array(n)
  for (let i = 0; i < n; i += 1) out[i] = hits[i].item
  return out
}

/**
 * 同分时的次序：短路径优先，再按路径字典序。
 *
 * 短路径优先不是审美：`foo/bar.md` 与 `a/b/c/foo/bar.md` 得分完全相同时，
 * 用户要的几乎总是浅的那个。没有这条，同分结果的顺序取决于
 * `Array.prototype.sort` 的实现细节，换个引擎就变。
 */
function compareHits(a: { item: QuickItem; score: number }, b: { item: QuickItem; score: number }): number {
  if (a.score !== b.score) return b.score - a.score
  if (a.item.path.length !== b.item.path.length) return a.item.path.length - b.item.path.length
  return a.item.path < b.item.path ? -1 : a.item.path > b.item.path ? 1 : 0
}
