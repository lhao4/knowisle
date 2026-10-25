import { useCallback, useEffect, useLayoutEffect, useState } from 'react'

/**
 * 主题：三态**偏好** + 两态**结果**。
 *
 * # 为什么是三态
 *
 * 只做「亮 / 暗」就必须选一个默认值，而选哪个都错一半：暗色系统里首次启动
 * 弹白屏是最刺眼的一类缺陷，用户会直接归因为「这软件不做暗色」。
 * `prefers-color-scheme` 在 WebView2 / WKWebView / WebKitGTK 上都有实现，
 * 零新依赖。代价是多一条「系统主题变了」的路径要测，按钮要能表达三种状态。
 *
 * # `system` 不进 DOM
 *
 * 写进 `<html data-theme>` 的只可能是 `light` / `dark`——`system` 在写之前就
 * 解析掉了。CSS 里不写 `[data-theme='system']`：那需要把整套暗色变量再在
 * `@media` 里写一遍，两处漂移是必然的。
 */
export type ThemePref = 'system' | 'light' | 'dark'
export type ThemeName = 'light' | 'dark'

/**
 * localStorage 键。
 *
 * **`index.html` 的内联脚本里也有一份字面量**，两处必须逐字一致——
 * 那份脚本是首帧不闪白的唯一保障，它不能 `import`（模块是异步的）。
 */
export const THEME_KEY = 'knowisle.theme'

/** 点击顺序。三下一循环，且第一下从「跟随系统」出发，最符合"试一下"的直觉。 */
const CYCLE: readonly ThemePref[] = ['system', 'light', 'dark']

const LABELS: Record<ThemePref, string> = {
  system: '跟随系统',
  light: '亮色',
  dark: '暗色',
}

export function isThemePref(value: unknown): value is ThemePref {
  return value === 'system' || value === 'light' || value === 'dark'
}

/**
 * 读偏好。**任何异常都回落到 `system`**。
 *
 * 隐私模式下 `localStorage` 的 getter 会直接抛（不是返回 null），
 * 而这条路径在启动关键路径上——不能让它把整个应用打断。
 * 非法值（手改过、旧版本遗留）同样回落到 `system`，不是回落到 `light`：
 * 用户没表达过偏好时，系统的选择就是最合理的那个。
 */
export function readThemePref(): ThemePref {
  try {
    const raw = localStorage.getItem(THEME_KEY)
    return isThemePref(raw) ? raw : 'system'
  } catch {
    return 'system'
  }
}

/** 系统当前是亮还是暗。`matchMedia` 缺失时按亮处理，与 CSS 的默认一致。 */
export function systemTheme(): ThemeName {
  if (typeof window.matchMedia !== 'function') return 'light'
  return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'
}

/** 把偏好解析成真正要落到 DOM 上的主题。 */
export function resolveTheme(pref: ThemePref, system: ThemeName): ThemeName {
  return pref === 'system' ? system : pref
}

/**
 * 把主题打到 `<html>` 上。
 *
 * 值没变就不写：给 `dataset` 赋同值也会触发一次属性变更与样式失效，
 * 而挂载时必然跑一次——首帧刚由内联脚本设好，这里不该再动它。
 * 这个「同值不写」也是验收里「`data-theme` 只在文档早期被写过一次」的前提。
 */
export function applyTheme(theme: ThemeName): void {
  const root = document.documentElement
  if (root.dataset.theme !== theme) root.dataset.theme = theme
}

/** 下一态。三态循环，永远回到起点。 */
export function nextThemePref(pref: ThemePref): ThemePref {
  const index = CYCLE.indexOf(pref)
  return CYCLE[(index + 1) % CYCLE.length]
}

export function themeLabel(pref: ThemePref): string {
  return LABELS[pref]
}

/**
 * 订阅系统主题。
 *
 * **无条件调用**：`pref` 不是 `system` 时它的值用不上，但条件调用 hook
 * 会打乱 hook 顺序（React 会直接报错）。代价是一个常开的 `change` 监听，
 * 它只在用户改系统设置时触发。
 */
function useSystemTheme(): ThemeName {
  const [theme, setTheme] = useState<ThemeName>(systemTheme)

  // 订阅用 `useEffect`（不是 layout）：它不影响这一帧画什么。
  useEffect(() => {
    if (typeof window.matchMedia !== 'function') return
    const query = window.matchMedia('(prefers-color-scheme: dark)')
    const onChange = (e: MediaQueryListEvent) => setTheme(e.matches ? 'dark' : 'light')
    query.addEventListener('change', onChange)
    return () => query.removeEventListener('change', onChange)
  }, [])

  return theme
}

export interface ThemeController {
  pref: ThemePref
  theme: ThemeName
  cycle: () => void
}

/**
 * 主题控制器。`App` 里只有一个实例，通过 props 把 `pref` / `cycle` 交给按钮。
 *
 * # 为什么是 `useLayoutEffect` 而不是 `useEffect`
 *
 * `useEffect` 在**浏览器绘制之后**才跑。点一下按钮，React 先把标签文字换成
 * 「暗色」，等这一帧画完才改 `data-theme`——于是每次切换都会有一帧
 * 「标签已经是暗色、界面还是亮色」。`useLayoutEffect` 在提交之后、绘制之前跑，
 * 两者落在同一帧里。
 *
 * # 为什么状态只许体现在 `dataset` 上
 *
 * `theme` 绝不能进 `NoteEditor` 的 props 或 `docId`：那会重建 `EditorState`，
 * 撤销栈、光标、选区一起归零（公理 A3：编辑态是独立管线，主题不该让它重配）。
 * 编辑器里所有颜色都走 `var(--ki-*)`，覆盖 L2 即自动跟随。
 */
export function useTheme(): ThemeController {
  const [pref, setPref] = useState<ThemePref>(readThemePref)
  const system = useSystemTheme()
  const theme = resolveTheme(pref, system)

  useLayoutEffect(() => {
    applyTheme(theme)
  }, [theme])

  const cycle = useCallback(() => {
    const next = nextThemePref(pref)
    try {
      localStorage.setItem(THEME_KEY, next)
    } catch {
      // 隐私模式 / 配额满：本次会话内照样生效，只是下次启动回到默认。
      // 不弹错——「主题记不住」不是需要用户处理的事。
    }
    setPref(next)
  }, [pref])

  return { pref, theme, cycle }
}
