import { describe, expect, it } from 'vitest'

import type { FileNode } from '../api'
import { QUICK_LIMIT, flattenVault, searchQuick, type QuickItem } from './quick-open'

/**
 * `quick-open.ts` 的行为契约。
 *
 * 这里钉的是**排序键**，不是"能搜到"。打分规则有三条（连续 > 分散、词首 >
 * 中间、大小写一致加分），它们互相能压过对方，所以每条都要有一个「只差这一条」
 * 的对照组——否则删掉任何一个权重，用例照样绿。
 */

const file = (name: string, path: string): FileNode => ({
  name,
  path,
  kind: 'file',
  size: 0,
  mtime: 0,
})

const dir = (name: string, path: string, children: FileNode[] | null): FileNode => ({
  name,
  path,
  kind: 'dir',
  size: 0,
  mtime: 0,
  children,
})

/** 直接造 `QuickItem`：排序用例不该被扁平化的排序顺序干扰。 */
const item = (path: string): QuickItem => {
  const slash = path.lastIndexOf('/')
  const base = slash < 0 ? path : path.slice(slash + 1)
  return {
    path,
    name: base.endsWith('.md') ? base.slice(0, -3) : base,
    dir: slash < 0 ? '' : path.slice(0, slash),
  }
}

const paths = (items: QuickItem[]): string[] => items.map((it) => it.path)

describe('flattenVault', () => {
  it('扁平化：只出文件条目，目录本身不占一行', () => {
    const tree = dir('', '', [file('a.md', 'a.md'), dir('b', 'b', [file('c.md', 'b/c.md')])])
    expect(paths(flattenVault(tree))).toEqual(['a.md', 'b/c.md'])
  })

  it('扁平化：保持树里的顺序，不重排', () => {
    // 输入故意违反码位序（`b.md` > `a.md`）：输出必须原样保持。
    // 排序是扫描器的事（`ki-vault/src/sort.rs`），前端再排一遍就是第二份顺序。
    const tree = dir('', '', [file('b.md', 'b.md'), file('a.md', 'a.md')])
    expect(paths(flattenVault(tree))).toEqual(['b.md', 'a.md'])
  })

  it('扁平化：目录展开在原地（先序），不是把所有文件攒完再排', () => {
    // `a` 目录排在 `a.md` 前面时，它里面的文件就出现在 `a.md` 之前。
    const tree = dir('', '', [dir('a', 'a', [file('x.md', 'a/x.md')]), file('a.md', 'a.md')])
    expect(paths(flattenVault(tree))).toEqual(['a/x.md', 'a.md'])
  })

  it('扁平化：name 去掉 `.md`，大小写不敏感', () => {
    const tree = dir('', '', [file('NOTE.MD', 'NOTE.MD')])
    expect(flattenVault(tree)[0].name).toBe('NOTE')
  })

  it('扁平化：没有 `.md` 后缀的名字原样保留', () => {
    const tree = dir('', '', [file('README', 'README')])
    expect(flattenVault(tree)[0].name).toBe('README')
  })

  it('扁平化：根目录下的文件 dir 是空串，子目录里的带上相对目录', () => {
    const tree = dir('', '', [file('c.md', 'c.md'), file('b.md', 'a/b.md')])
    const items = flattenVault(tree)
    expect(items.map((it) => [it.path, it.dir])).toEqual([
      ['c.md', ''],
      ['a/b.md', 'a'],
    ])
  })

  it('扁平化：目录的 children 为 null 时跳过，不抛', () => {
    const tree = dir('', '', [dir('empty', 'empty', null), file('a.md', 'a.md')])
    expect(paths(flattenVault(tree))).toEqual(['a.md'])
  })

  it('扁平化：136 层深（扫描器实测上限）不丢条目', () => {
    const depth = 136
    const segments = Array.from({ length: depth }, (_, i) => `d${i}`)
    let node = file('leaf.md', `${segments.join('/')}/leaf.md`)
    for (let i = depth - 1; i >= 0; i -= 1) {
      node = dir(segments[i], segments.slice(0, i + 1).join('/'), [node])
    }
    expect(flattenVault(node)).toHaveLength(1)
  })
})

describe('searchQuick · 空查询', () => {
  it('空查询：按已排好的顺序返回前 limit 篇', () => {
    const items = [item('a.md'), item('b.md'), item('c.md')]
    expect(paths(searchQuick(items, '', 2))).toEqual(['a.md', 'b.md'])
  })

  it('空查询：limit 大于总数时返回全部', () => {
    const items = [item('a.md'), item('b.md')]
    expect(searchQuick(items, '', 10)).toHaveLength(2)
  })

  it('空查询：默认上限是 QUICK_LIMIT（50）', () => {
    const items = Array.from({ length: 60 }, (_, i) => item(`n${String(i).padStart(2, '0')}.md`))
    expect(searchQuick(items, '')).toHaveLength(QUICK_LIMIT)
    expect(QUICK_LIMIT).toBe(50)
  })

  it('空查询：空库返回空数组', () => {
    expect(searchQuick([], '')).toEqual([])
  })
})

describe('searchQuick · 匹配', () => {
  it('子序列：`foo/bar` 命中 `foo/bar.md`', () => {
    expect(paths(searchQuick([item('foo/bar.md')], 'foo/bar'))).toEqual(['foo/bar.md'])
  })

  it('子序列：跳过字符也算命中（`fbr` → `foo/bar.md`）', () => {
    expect(paths(searchQuick([item('foo/bar.md')], 'fbr'))).toEqual(['foo/bar.md'])
  })

  it('大小写折叠：`FOO` 命中 `foo/bar.md`', () => {
    expect(paths(searchQuick([item('foo/bar.md')], 'FOO'))).toEqual(['foo/bar.md'])
  })

  it('未命中：不返回任何项', () => {
    expect(searchQuick([item('foo/bar.md')], 'zzz')).toEqual([])
  })

  it('查询比可匹配窗口长时返回空（`md` 不该命中整个库）', () => {
    expect(searchQuick([item('abc.md')], 'md')).toEqual([])
  })

  it('空库对任何查询都返回空数组', () => {
    expect(searchQuick([], 'x')).toEqual([])
  })
})

describe('searchQuick · 排序', () => {
  it('连续命中优先：`xab.md` 排在 `xacb.md` 前面', () => {
    // 两者都没有词首加分，差别只在「相邻」与「隔一个字符」。
    expect(paths(searchQuick([item('xacb.md'), item('xab.md')], 'ab'))).toEqual([
      'xab.md',
      'xacb.md',
    ])
  })

  it('词首命中优先：`a-b.md` 排在 `xab.md` 前面', () => {
    expect(paths(searchQuick([item('xab.md'), item('a-b.md')], 'ab'))).toEqual([
      'a-b.md',
      'xab.md',
    ])
  })

  it('大小写一致加分：查询 `API` 时 `API-Design.md` 排在 `api-design.md` 前面', () => {
    expect(paths(searchQuick([item('api-design.md'), item('API-Design.md')], 'API'))).toEqual([
      'API-Design.md',
      'api-design.md',
    ])
  })

  it('同分时短路径优先：`a/foo.md` 排在 `a/b/foo.md` 前面', () => {
    // 两篇得分完全相同时，用户要的几乎总是浅的那个；不靠 sort 的实现细节。
    expect(paths(searchQuick([item('a/b/foo.md'), item('a/foo.md')], 'foo'))).toEqual([
      'a/foo.md',
      'a/b/foo.md',
    ])
  })

  it('limit 截断：命中多于 limit 时只返回 limit 条', () => {
    const items = ['ab.md', 'ac.md', 'ad.md'].map(item)
    expect(searchQuick(items, 'a', 2)).toHaveLength(2)
  })

  it('limit 为 0 时返回空数组', () => {
    expect(searchQuick([item('a.md')], 'a', 0)).toEqual([])
  })
})

describe('flattenVault + searchQuick', () => {
  it('整条链路：树 → 扁平列表 → 查询命中', () => {
    const tree = dir('', '', [
      dir('notes', 'notes', [file('design.md', 'notes/design.md')]),
      file('inbox.md', 'inbox.md'),
    ])
    expect(paths(searchQuick(flattenVault(tree), 'ndes'))).toEqual(['notes/design.md'])
  })
})
