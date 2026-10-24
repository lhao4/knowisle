import { useState } from 'react'
import type { FileNode } from '../api'

interface TreeNodeProps {
  node: FileNode
  depth: number
  activePath: string | null
  onSelect: (node: FileNode) => void
}

/**
 * 单个树节点。
 *
 * 展开状态放在节点自己身上（而不是父组件的 Map 里），
 * 因为 M0 的树是一次性全量渲染的：1000 个节点的 useState 成本可以忽略，
 * 换来的是"改一个节点不会让整棵树重渲染"。
 * 等节点数超过 3000 再换成集中状态 + 虚拟滚动。
 */
export function TreeNode({ node, depth, activePath, onSelect }: TreeNodeProps) {
  // 只自动展开"第一层且子节点不多"的目录。
  // 无条件展开第一层会让 20 个顶层目录各自摊开近千个文件——
  // 首屏渲染从 30ms 变成 300ms，而这个代价换来的只是"少点两下"。
  const [expanded, setExpanded] = useState(
    depth === 0 && (node.children?.length ?? 0) <= 8,
  )

  const indent = { paddingLeft: `${10 + depth * 14}px` }

  if (node.kind === 'file') {
    const isActive = node.path === activePath
    return (
      <button
        type="button"
        className={`tree-row tree-row--file${isActive ? ' is-active' : ''}`}
        style={indent}
        onClick={() => onSelect(node)}
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
          onSelect={onSelect}
        />
      ))}
    </div>
  )
}
