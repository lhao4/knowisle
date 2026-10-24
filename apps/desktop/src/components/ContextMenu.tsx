import { useEffect, useLayoutEffect, useRef } from 'react'

export interface ContextMenuItem {
  label: string
  onSelect: () => void
}

interface ContextMenuProps {
  /** 鼠标位置（`clientX` / `clientY`）。 */
  x: number
  y: number
  items: ContextMenuItem[]
  onClose: () => void
}

/** 估算尺寸，用于贴边时把菜单拉回视口内。 */
const ITEM_HEIGHT = 30
const MENU_WIDTH = 168
const EDGE = 8

/**
 * 树节点的右键菜单。
 *
 * # 为什么自己写
 *
 * 原生 `<menu>` 没有"在指定坐标弹出"的 API；Electron 的 `Menu.popup()` 在
 * Tauri 里要走另一套权限与命令，为两个菜单项引入一条新的 IPC 通道不划算。
 * 一个 fixed 的 div 就够，而且焦点、Esc、点外关闭的行为与其它浮层一致。
 *
 * # 为什么要监听 wheel / resize
 *
 * 菜单是 fixed 定位，但触发它的那个树节点会随着侧栏滚动移走。不关掉的话，
 * 用户滚一下滚轮，菜单就悬在离节点很远的地方——点下去改的是另一篇笔记。
 * 与其跟踪节点的位置（那要每帧量 DOM），不如直接收起来。
 */
export function ContextMenu({ x, y, items, onClose }: ContextMenuProps) {
  const menuRef = useRef<HTMLDivElement>(null)

  // 贴边收回：右键点在最右侧/最底部时，菜单不能跑出窗口。
  const left = Math.max(EDGE, Math.min(x, window.innerWidth - MENU_WIDTH - EDGE))
  const top = Math.max(
    EDGE,
    Math.min(y, window.innerHeight - items.length * ITEM_HEIGHT - EDGE),
  )

  useLayoutEffect(() => {
    // 焦点落到第一项：这样 Esc 与方向键有明确的归属，Tab 也不会甩到正文里。
    menuRef.current?.querySelector('button')?.focus()
  }, [])

  useEffect(() => {
    // 捕获阶段：树节点自己的 mousedown 先于冒泡，不能被它吞掉。
    const onPointerDown = (e: globalThis.MouseEvent) => {
      if (menuRef.current !== null && e.target instanceof Node && menuRef.current.contains(e.target)) {
        return
      }
      onClose()
    }
    const onKeyDown = (e: globalThis.KeyboardEvent) => {
      if (e.key !== 'Escape') return
      e.preventDefault()
      onClose()
    }
    window.addEventListener('mousedown', onPointerDown, true)
    window.addEventListener('keydown', onKeyDown, true)
    window.addEventListener('resize', onClose)
    window.addEventListener('wheel', onClose, { passive: true })
    window.addEventListener('blur', onClose)
    return () => {
      window.removeEventListener('mousedown', onPointerDown, true)
      window.removeEventListener('keydown', onKeyDown, true)
      window.removeEventListener('resize', onClose)
      window.removeEventListener('wheel', onClose)
      window.removeEventListener('blur', onClose)
    }
  }, [onClose])

  return (
    <div
      ref={menuRef}
      className="context-menu"
      role="menu"
      style={{ left, top, width: MENU_WIDTH }}
    >
      {items.map((item) => (
        <button
          key={item.label}
          type="button"
          role="menuitem"
          className="context-menu__item"
          onClick={() => {
            // 先收菜单再执行：动作可能开一个新浮层，两层叠在一起时
            // Esc 该关谁就没有答案了。
            onClose()
            item.onSelect()
          }}
        >
          {item.label}
        </button>
      ))}
    </div>
  )
}
