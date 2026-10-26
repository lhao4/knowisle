/**
 * 回收站列表的展示层换算。纯函数，不碰 IPC。
 *
 * 只放**前端独有**的换算。展示名与原路径的判定留在 Rust（`TrashEntry.name`
 * 已经算好了）：`<unix_ms>-` 的剥法若两边各写一份，迟早会在某个边界上不一致，
 * 而那种不一致不报错，只会让列表里的名字和恢复出来的不一样。
 */

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
