/**
 * 窗口重新获得焦点时，「编辑器里的这一篇」与「磁盘上的这一篇」还对不对得上。
 *
 * 纯函数：不碰 DOM、不 import 组件、不调 IPC。判据全部来自参数，所以能在
 * `environment: 'node'` 里逐条钉死——这里的每一个分支都对应一次「用户被误报」
 * 或者「用户被静默覆盖」，值得单独测。
 *
 * # 为什么不是 watcher
 *
 * MVP-SCOPE §3.6 明确砍掉了 `notify` crate：三平台的事件语义各不相同
 * （Windows 的「先删后建」、macOS 的 FSEvents 合并、Linux 的 inotify 上限），
 * 每一套都要单独调试，而外部编辑在 MVP 阶段是低频事件。聚焦时校验覆盖
 * 95% 的场景，成本是 1/10。见 ADR-011。
 *
 * # 三条判据的优先级
 *
 * 1. **mtime 是主判据**：它没动过，就认为磁盘没被写过。反过来（只信内容）
 *    会把「用户刚敲、300ms 合并窗口还没发出去的字」当成外部改动，一天误报几十次。
 * 2. **内容相同 → 只推基线**：外部工具原样重存（或第九刀的转存）会换 mtime
 *    而不换内容。这时候弹横幅是噪音，但**必须**把基线推到新 mtime，
 *    否则下一次保存必然 `CONFLICT`——那是一个用户完全无法理解的假冲突。
 * 3. **mtime 没动、内容却不同 → 只在「没有任何未落盘改动」时才认**：
 *    这是「外部写入落在同一毫秒」的兜底（Windows 的 mtime 粒度实测够粗）。
 *    一旦 `dirty` 为真，内容不同就有了另一个完全正常的解释——用户自己的字。
 */

/** 一次校验的输入。字段的取法见 `App.tsx` 的 `checkExternalChange`。 */
export type RevalidateInput = {
  /** 编辑器里现在的正文（含 300ms 合并窗口里还没发出去的那部分）。 */
  editorContent: string
  /** `noteRead` 回来的正文。 */
  diskContent: string
  /** `noteRead` 回来的 mtime。 */
  diskMtime: number
  /**
   * 我们相信磁盘上那一版的 mtime。
   *
   * 优先取 `noteStatus().lastSavedMtime`（Rust 槽位的真值），退回
   * `activeNote.mtime`（打开时读到的）。**不能**用 `saveState` 里的那份：
   * 它靠 500ms 轮询刷新，落盘之后最多滞后半秒，而「刚保存完又敲了一个字」
   * 恰好落在这个窗口里，会把用户自己的输入误判成外部改动。
   */
  baselineMtime: number
  /**
   * 有没有还没落盘的改动。
   *
   * 必须把**编辑器缓冲**也算进来（`NoteEditorHandle.hasPending`）：
   * `noteStatus().dirty` 只反映 Rust 槽位，而按键到 `noteEdit` 之间有
   * 300ms 的合并窗口。少了这一半，「打完字立刻切窗再回来」会被误报。
   */
  dirty: boolean
}

/** 校验结论。 */
export type Verdict =
  /** 磁盘没动过，或者动的只是 mtime 而内容一致且已对齐。 */
  | { kind: 'noop' }
  /** 内容相同、mtime 变了：不打扰用户，只把基线推到新 mtime。 */
  | { kind: 'adopt-baseline'; mtime: number }
  /** 真的分叉了：弹横幅，由用户决定换不换。`dirty` 决定文案与是否二次确认。 */
  | { kind: 'prompt'; dirty: boolean }

/**
 * 判断一次聚焦校验的结论。
 *
 * **永远不返回「自动重载」**：外部改动只提示，换不换由用户点。
 * 自动重载会吞掉他没保存的字——那是比「看到旧内容」严重得多的事故。
 */
export function revalidate(input: RevalidateInput): Verdict {
  const { editorContent, diskContent, diskMtime, baselineMtime, dirty } = input
  const sameContent = diskContent === editorContent

  if (diskMtime === baselineMtime) {
    // mtime 没动。内容不同只可能是「用户自己还没发出去的字」（dirty），
    // 或者外部写入恰好落在同一毫秒（极罕见，见文件头第 3 条）。
    if (dirty || sameContent) return { kind: 'noop' }
    return { kind: 'prompt', dirty: false }
  }

  if (sameContent) {
    // 内容一致但 mtime 变了：外部工具原样保存、git checkout、第九刀的转存。
    // 不弹横幅，但基线必须前进——否则下一次保存会拿着过期的 mtime 撞冲突。
    return { kind: 'adopt-baseline', mtime: diskMtime }
  }

  return { kind: 'prompt', dirty }
}
