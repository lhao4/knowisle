import type { FileNode } from '../api'

/**
 * 树里有没有这个路径（**精确匹配**，不是前缀匹配）。
 *
 * # 它服务的那一条判据
 *
 * 重扫之后要回答「我正在编辑的这一篇还在不在树里」（ADR-017）。这个判断
 * 只能拿**重扫刚返回的那棵树**来算：`activeNote` 是内存里的旧事实，树是
 * 磁盘上的新事实，两者分叉正是要提示的东西。
 *
 * # 为什么不是 `path.startsWith`
 *
 * `notes/a.md` 与 `notes/ab.md` 共享前缀。用前缀判断会让「a.md 被删了、
 * ab.md 还在」判成「还在」，提示就永远不出现——而那是这个函数唯一的用途。
 *
 * # 成本
 *
 * 一次 DFS，10k 节点实测在微秒级（只在一次重扫结束时调用一次）。树不排序、
 * 不复制、不缓存：任何缓存都会引入「缓存的树是哪一次扫描的」这个问题。
 */
export function hasPath(node: FileNode, path: string): boolean {
  if (node.path === path) return true
  const children = node.children
  if (children === undefined || children === null) return false
  for (const child of children) {
    if (hasPath(child, path)) return true
  }
  return false
}
