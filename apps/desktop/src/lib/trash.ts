/**
 * 回收站列表的展示层换算。纯函数，不碰 IPC。
 *
 * 只放**前端独有**的换算。展示名与原路径的判定留在 Rust（`TrashEntry.name`
 * 已经算好了）：`<unix_ms>-` 的剥法若两边各写一份，迟早会在某个边界上不一致，
 * 而那种不一致不报错，只会让列表里的名字和恢复出来的不一样。
 */

/**
 * 一次「加载更多」取多少条。
 *
 * 与服务端的 `DEFAULT_PAGE_LIMIT` 一致（`ki-vault/src/trash.rs`）。服务端还会
 * 把上限钳到 500，所以这里写大了也越不过去——但写大了会让首屏读一堆看不见的行。
 */
export const TRASH_PAGE_SIZE = 200

/**
 * 面板里最多累积多少行。
 *
 * 这不是分页大小，是**渲染上限**：10000 个 `<li>` 塞进面板会让滚动掉帧，
 * 而用户翻到第 1000 条还没找到东西时，该做的是搜索（B-29 的第 ② 步），
 * 不是继续往下翻。到顶之后按钮变成一句陈述，不再发请求。
 */
export const TRASH_MAX_ROWS = 1000

/**
 * 删除时间。
 *
 * 精确到分钟：回收站里没人关心秒，而"2026/10/25 22:52:12"这种全量格式会把
 * 每行撑长一倍，把原路径挤到换行。跨年时补上年份——「1月3日」在两年前那次
 * 删除上是有歧义的。
 */
export function formatDeletedAt(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return '时间未知'
  const at = new Date(ms)
  if (Number.isNaN(at.getTime())) return '时间未知'

  const now = new Date()
  const sameYear = at.getFullYear() === now.getFullYear()
  return at.toLocaleString('zh-CN', {
    ...(sameYear ? {} : { year: 'numeric' }),
    month: 'numeric',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  })
}
