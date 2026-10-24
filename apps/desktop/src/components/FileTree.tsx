import { useEffect, useState, type MouseEvent } from 'react'
import type { FileNode } from '../api'

interface TreeNodeProps {
  node: FileNode
  depth: number
  activePath: string | null
  /**
   * 新建 / 改名之后需要在树里**露出来**的路径。
   *
   * 命中的目录会自动展开一次。没有它的话，在折叠的目录里新建一篇笔记，
   * 用户只看到编辑器换了内容、侧栏毫无反应——"树里立刻出现"这条验收
   * 就变成了"取决于那个目录当时是不是展开的"。
   *
   * 只在值变化时展开一次：用户随后手动折叠，不该被下一次无关重渲染弹开。
   */
  revealPath: string | null
  onSelect: (node: FileNode) => void
  onContextMenu: (node: FileNode, x: number, y: number) => void
}

/**
 * 单个树节点。
 *
 * 展开状态放在节点自己身上（而不是父组件的 Map 里），
 * 因为 M0 的树是一次性全量渲染的：1000 个节点的 useState 成本可以忽略，
 * 换来的是"改一个节点不会让整棵树重渲染"。
 * 等节点数超过 3000 再换成集中状态 + 虚拟滚动。
 */
export function TreeNode({
  node,
  depth,
  activePath,
  revealPath,
  onSelect,
  onContextMenu,
}: TreeNodeProps) {
  // 只自动展开"第一层且子节点不多"的目录。
  // 无条件展开第一层会让 20 个顶层目录各自摊开近千个文件——
  // 首屏渲染从 30ms 变成 300ms，而这个代价换来的只是"少点两下"。
  const [expanded, setExpanded] = useState(
    depth === 0 && (node.children?.length ?? 0) <= 8,
  )

  // 路径是节点自身或其子孙 → 需要展开到能看见它。
  //
  // 这里刻意用 effect 而不是"渲染期派生 `expanded || reveal`"：
  // 派生写法会让被露出来的目录**再也折不起来**（reveal 一直在，展开态就一直是
  // true），而 effect 只在 reveal 变化的那一次强制展开，之后用户的手动折叠说了算。
  // 触发源在上层（新建/改名），不在这个组件里，所以也没有"事件里 setState"可用。
  const reveal = revealPath !== null && revealPath.startsWith(`${node.path}/`)
  useEffect(() => {
    // oxlint-disable-next-line react/set-state-in-effect -- 见上：这是对上层事件的同步，不是可派生状态
    if (reveal) setExpanded(true)
  }, [reveal])

  const indent = { paddingLeft: `${10 + depth * 14}px` }

  /** 右键：把坐标交给上层，菜单本身渲染在 App 里（它是 fixed 定位）。 */
  const handleContextMenu = (e: MouseEvent) => {
    e.preventDefault()
    onContextMenu(node, e.clientX, e.clientY)
  }

  if (node.kind === 'file') {
    const isActive = node.path === activePath
    return (
      <button
        type="button"
        className={`tree-row tree-row--file${isActive ? ' is-active' : ''}`}
        style={indent}
        onClick={() => onSelect(node)}
        onContextMenu={handleContextMenu}
        title={node.path}
      >
        <span className="tree-caret tree-caret--placeholder" aria-hidden="true" />
        <span className="tree-label">{node.name}</span>
      </button>
    )
  }

  const childCount = node.children?.length ?? 0

  return (
    <div className="tree-branch">
      <button
        type="button"
        className="tree-row tree-row--dir"
        style={indent}
        onClick={() => setExpanded((value) => !value)}
        onContextMenu={handleContextMenu}
        title={node.path || '(Vault 根目录)'}
        aria-expanded={expanded}
      >
        <span className={`tree-caret${expanded ? ' is-open' : ''}`} aria-hidden="true">
          ▸
        </span>
        <span className="tree-label">{node.name || 'Vault'}</span>
        <span className="tree-count">{childCount}</span>
      </button>

      {expanded && node.children?.map((child) => (
        <TreeNode
          key={child.path}
          node={child}
          depth={depth + 1}
          activePath={activePath}
          revealPath={revealPath}
          onSelect={onSelect}
          onContextMenu={onContextMenu}
        />
      ))}
    </div>
  )
}
