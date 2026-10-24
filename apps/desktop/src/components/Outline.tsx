import { memo, useCallback, useImperativeHandle, useLayoutEffect, useRef, type Ref } from 'react'

import type { OutlineItem } from '../lib/outline'

/**
 * 大纲面板的**命令式**接口。
 *
 * # 为什么不是 state
 *
 * 光标每动一次，当前章节就变一次；连打 100 个字符，它变 100 次。
 * 如果 `activeIndex` 走 `setState`，这 100 次就是 100 次 React 渲染 +
 * 100 次对 500 个节点的 reconcile——而屏幕上真正变化的只有一个 class。
 *
 * 所以高亮走这条路：调用方直接拿到面板的句柄，面板只做
 * `classList.toggle`，React 完全不参与。`items` 是另一条路（低频，走 props）。
 * **两条路不能合并**，合并的那一刻渲染次数就与按键次数同阶了。
 */
export interface OutlineHandle {
  /** 改高亮项。同一个下标重复调用是空操作。不滚动面板。 */
  setActive: (index: number) => void
  /** 把当前高亮项滚进可视区（`block: 'nearest'`）。只在编辑器滚动时调。 */
  revealActive: () => void
}

interface OutlineProps {
  /** 低频：只有标题集合变化时才换。 */
  items: OutlineItem[]
  /** 点击某一项：把光标送过去并滚动编辑器。 */
  onJump: (pos: number) => void
  ref?: Ref<OutlineHandle>
}

/**
 * 右侧大纲面板。
 *
 * 三条约定：
 *
 * 1. **列表 DOM 的顺序与 `items` 一一对应。** `setActive` 靠
 *    `children[index]` 定位元素，所以列表容器里不许再塞别的东西
 *    （空状态是列表的**兄弟节点**，不是子节点）。
 * 2. **`items` 换了之后必须重挂类名。** React 会重建子节点，`activeElRef`
 *    指向的是已经脱离文档的旧元素；`useLayoutEffect` 在 DOM 提交后、绘制前
 *    重新按 `activeRef` 挂一次，用户看不到中间态。
 * 3. **不反向同步。** 面板自己滚动不改高亮——高亮只由光标决定。
 *    这是 MVP-SCOPE 点名的抖动来源：两边都想控制同一个状态，
 *    滚动时高亮跳来跳去。
 *
 * # 为什么套 `memo`
 *
 * 上层 `App` 会因为保存状态（`dirty` → 轮询 → `setSaveState`）每 300–500ms
 * 重渲染一次，那是 M1 的既有路径，与本功能无关。但 React 默认会连带重渲染
 * 子组件，于是 500 项大纲会在用户打字时被反复 reconcile——渲染次数就与
 * 「按键次数」同阶了，正是本功能要避免的。
 *
 * 套上 `memo` 之后，只有 `items` 换引用（= 标题集合真的变了）才重渲染。
 * `onJump` 由上层 `useCallback` 固定，`ref` 在 React 19 是普通 prop。
 */
export const Outline = memo(function Outline({ items, onJump, ref }: OutlineProps) {
  const listRef = useRef<HTMLDivElement>(null)
  /** 当前高亮下标。-1 表示没有（光标在第一个标题之前）。 */
  const activeRef = useRef(-1)
  /** 当前挂了 `is-active` 的那个 DOM 节点，用来在下次切换时摘掉。 */
  const activeElRef = useRef<HTMLElement | null>(null)

  /**
   * 把类名对齐到 `activeRef`。
   *
   * 只碰「上一个」和「这一个」两个节点，不遍历整张列表：500 项时逐项
   * `classList.toggle` 是 500 次 DOM 读写，而实际变化最多两处。
   */
  const paint = useCallback(() => {
    const list = listRef.current
    const index = activeRef.current
    const next =
      list !== null && index >= 0
        ? ((list.children[index] as HTMLElement | undefined) ?? null)
        : null

    if (next === activeElRef.current) return
    activeElRef.current?.classList.remove('is-active')
    next?.classList.add('is-active')
    activeElRef.current = next
  }, [])

  useImperativeHandle(
    ref,
    () => ({
      setActive: (index: number) => {
        if (index === activeRef.current) return
        activeRef.current = index
        paint()
      },
      revealActive: () => {
        // 已经可见时浏览器什么都不做，所以每帧调也安全。
        // `nearest` 是关键：`start` 会让面板每帧都跳一下。
        activeElRef.current?.scrollIntoView({ block: 'nearest' })
      },
    }),
    [paint],
  )

  // 依赖 `items` 的**引用**：上层只在标题集合变化时换新数组。
  useLayoutEffect(() => {
    // 旧元素已经随上一次渲染被丢弃，不能再对它 remove；直接重新定位。
    activeElRef.current = null
    paint()
  }, [items, paint])

  return (
    <aside className="outline" aria-label="大纲">
      <div className="outline__header">大纲</div>
      {items.length === 0 ? (
        <p className="outline__empty">本篇没有标题</p>
      ) : (
        <div className="outline__list" ref={listRef}>
          {items.map((item, index) => (
            <button
              // 用下标做 key 是刻意的：大纲是**序列**，一项的身份就是它在
              // 序列里的位置。`pos` 会随正文编辑漂移（它是绝对偏移），
              // 拿它做 key 会让「在上面打一个字」把整张列表重建一遍。
              key={index}
              type="button"
              className="outline__item"
              data-level={item.level}
              // 缩进跟着级别走，用内联样式：6 个级别写成 6 条 CSS 规则
              // 只是把同一个数字抄六遍，而且改档距要改六处。
              style={{ paddingLeft: 12 + (item.level - 1) * 12 }}
              title={item.text}
              onClick={() => onJump(item.pos)}
            >
              {item.text}
            </button>
          ))}
        </div>
      )}
    </aside>
  )
})
