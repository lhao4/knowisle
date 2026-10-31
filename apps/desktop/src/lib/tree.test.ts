/**
 * `hasPath` 的用例。
 *
 * # 为什么这几条值得单独钉
 *
 * 它只有一个调用点（重扫之后的对账，ADR-017），但判断错了的两种表现都很贵：
 *
 * - **假阳性**（明明没了却说在）→ 提示永远不出现，用户继续在「树里没有、
 *   磁盘上也没有」的笔记上打字，直到保存被拒绝才知道。
 * - **假阴性**（明明在却说没了）→ 每次重扫都弹一条假警告，用户会学会忽略
 *   所有警告。
 *
 * 前缀那条（`notes/a.md` vs `notes/ab.md`）是唯一一个用 `startsWith` 也会
 * 「看起来对」的写法，所以它必须有一条自己的用例。
 */

import { describe, expect, it } from 'vitest'

import type { FileNode } from '../api'
import { hasPath } from './tree'

const dir = (path: string, children: FileNode[]): FileNode => ({
  path,
  name: path.split('/').pop() ?? path,
  kind: 'dir',
  size: 0,
  mtime: 0,
  children,
})

const file = (path: string): FileNode => ({
  path,
  name: path.split('/').pop() ?? path,
  kind: 'file',
  size: 1,
  mtime: 1,
})

const tree = dir('', [
  dir('notes', [file('notes/a.md'), file('notes/ab.md'), dir('notes/sub', [file('notes/sub/deep.md')])]),
  file('root.md'),
])

describe('hasPath', () => {
  it('命中根节点', () => {
    expect(hasPath(tree, '')).toBe(true)
  })

  it('命中直接子节点', () => {
    expect(hasPath(tree, 'root.md')).toBe(true)
  })

  it('命中深层节点', () => {
    expect(hasPath(tree, 'notes/sub/deep.md')).toBe(true)
  })

  it('不存在的路径返回 false', () => {
    expect(hasPath(tree, 'notes/gone.md')).toBe(false)
    expect(hasPath(tree, 'gone')).toBe(false)
  })

  it('精确匹配：前缀相同的兄弟不算命中', () => {
    expect(hasPath(tree, 'notes/a')).toBe(false)
    expect(hasPath(tree, 'notes/ab.md')).toBe(true)
  })

  it('空目录里什么都没有（除了它自己）', () => {
    const empty = dir('only', [])
    expect(hasPath(empty, 'only')).toBe(true)
    expect(hasPath(empty, 'only/x.md')).toBe(false)
  })
})
