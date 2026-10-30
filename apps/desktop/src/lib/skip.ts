/**
 * 跳过清单的**纯逻辑**：把原因枚举翻成两句人话，把路径翻成「能不能照抄」。
 *
 * 放在 `lib/` 而不是面板组件里，理由与 `lib/encoding.ts` 相同：这些文案有
 * 必须被断言的约束（九种原因必须九句不同的话、lossy 路径必须带标记），
 * 而组件测试在本项目里不做（ARCHITECTURE 1.7）。
 *
 * # 为什么文案里必须有「为什么不能自动修」
 *
 * 面板的读者只有一个问题：「我该做什么」。一个只写「没有读权限」的清单，
 * 用户会去应用里找那个不存在的「修复」按钮——而这一刀刻意不做那个按钮
 * （§5 红线 2）：跳过的东西大多不可自动修，权限要用户自己改，GBK 文件名
 * 要用户自己改名，一个「一键修复」按钮只能删文件。
 *
 * 所以每条原因都是**两句话**：发生了什么（`what`），以及为什么应用帮不上 /
 * 用户该做什么（`fix`）。
 */

import type { IndexSkipReason, ScanSkipEntry, ScanSkipReason } from '../api'

/** 一条原因的两句话。 */
export interface SkipCopy {
  /** 发生了什么。一句，不带路径。 */
  what: string
  /** 为什么不能自动修 / 用户该做什么。 */
  fix: string
}

/**
 * 扫描侧（条目）的原因。
 *
 * # 五条必须互不相同
 *
 * 判据是「四类必须显示四句不同的人话」。两条原因共用一句话，等于把
 * 「原因」这个字段降级成一个无信息量的标签——那正是本刀要消灭的东西。
 *
 * 穷尽性靠**没有 `default`** + 末尾 `never`：Rust 侧加了新变体，这里会
 * 直接编译失败，而不是让新原因静默落到一句兜底话术。
 */
export function describeScanSkip(reason: ScanSkipReason): SkipCopy {
  switch (reason) {
    case 'permission':
      return {
        what: '没有读权限，这个条目打不开。',
        fix: '权限要你在系统里改（或在别处放一份），应用不会替你改 ACL——它也不该有那个权力。',
      }
    case 'nonUtf8Name':
      return {
        what: '文件名里有无法用 UTF-8 表示的字节，读不出它的真名字。',
        fix: '只有你知道它本来叫什么。别照下面那行去改名——那是带替换字符的替身，照抄会找不到文件；请到文件夹里按位置找它。',
      }
    case 'notARegularFile':
      return {
        what: '它不是普通文件，也不是目录（软链、管道、设备文件）。',
        fix: '扫描只收普通 .md。软链要跟得改架构（B-08），管道和设备要不要留是你的决定，应用不替你做。',
      }
    case 'walkError':
      return {
        what: '遍历到它的时候出错了，没读成。',
        fix: '典型是扫描途中它被别的程序删掉或改名了。再扫一次通常就没了，不需要你做什么。',
      }
    case 'internal':
      return {
        what: '内部判定不一致，跳过了它。',
        fix: '这是我们的 bug，不是你的文件有问题。它只计入总数、不进清单——列出来只会让你以为能修。',
      }
  }
  const _exhaustive: never = reason
  void _exhaustive
}

/**
 * 索引侧（笔记）的原因。
 *
 * # `invalidUtf8` 与 `nonUtf8Name` 不是一件事
 *
 * 前者是**正文**认不出编码（这篇搜不到），后者是**文件名**不是 UTF-8
 * （这个条目进不了树）。两个名字长得像，动作完全不同，文案必须分开写。
 *
 * # `invalidUtf8` 也与「待转存」不是一件事
 *
 * 待转存那批是**读得出来**、只是磁盘编码不是 UTF-8 的笔记（GBK 等）。
 * 它们能进索引、能被搜到，动作是「转存」。这里的 `invalidUtf8` 是连读都
 * 读不出来，转存无从下手——所以文案里明说「转存帮不上」。
 */
export function describeIndexSkip(reason: IndexSkipReason): SkipCopy {
  switch (reason) {
    case 'tooLarge':
      return {
        what: '超过单篇 2 MB 的索引上限，没有进索引。',
        fix: '拆分这篇，或把大附件挪到别的文件里，它才会被索引。应用不会自动截断你的正文——那等于替你丢字。',
      }
    case 'invalidUtf8':
      return {
        what: '正文认不出编码，读不出来。',
        fix: '转存帮不上：那要先能读出来。用别的编辑器确认它到底是什么编码，另存成 UTF-8 再扫描。',
      }
    case 'io':
      return {
        what: '读取时 IO 失败。',
        fix: '多半是文件被别的程序占着，或者权限不足。关掉那个程序后重新扫描库。',
      }
    case 'unknown':
      return {
        what: '索引里记着一个认不出的跳过原因。',
        fix: '索引库可能被外部改过。重开这个库会重建索引，原因会重新判定。',
      }
  }
  const _exhaustive: never = reason
  void _exhaustive
}

/** 清单里一行路径的显示文本。 */
export interface SkipPathLabel {
  /** 要显示的那一行。 */
  text: string
  /**
   * 这一行**不是**真名字（`to_string_lossy` 的产物，含 `U+FFFD`）。
   *
   * 为真时 UI 必须加一句「名字含无法显示的字符」——让用户照着它去改名，
   * 等于让他去找一个不存在的文件。
   */
  lossy: boolean
}

/**
 * 扫描侧那一条的路径标签。
 *
 * `path` 为 `null` 时给一句人话而不是空串：`walkdir` 的错误分支可能连路径
 * 都拿不到（`Loop` 变体只给祖先与子节点）。空着会让那一行看起来像渲染坏了。
 */
export function scanSkipPath(entry: ScanSkipEntry): SkipPathLabel {
  if (entry.path === null) return { text: '（没有路径：遍历器没能给出这一条）', lossy: false }
  return { text: entry.path, lossy: entry.pathLossy }
}
