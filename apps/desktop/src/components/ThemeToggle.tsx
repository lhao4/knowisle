import { themeLabel, type ThemePref } from '../lib/theme'

/**
 * 三态主题的循环切换按钮。
 *
 * # 为什么带文字
 *
 * 三态没法用一个开关表达。纯图标只能表达「现在是什么」或「点下去会变成什么」，
 * 二选一，怎么选都有一半是反的；而 `◐` 这种"半明半暗"的符号对普通用户
 * 等于没说。所以图标只做辅助，**状态由文字说清楚**。
 *
 * # 为什么在顶栏
 *
 * 它是 `.topbar__actions` 里唯一的无条件渲染项。侧栏在「还没打开库」时整块是
 * placeholder（`App.tsx` 里那个分支），按钮挂那里会在首次启动时消失——
 * 而那正是最需要切主题的时刻（系统是暗的、应用是亮的）。
 */
const ICONS: Record<ThemePref, string> = {
  system: '◐',
  light: '☀',
  dark: '☾',
}

export function ThemeToggle({ pref, onCycle }: { pref: ThemePref; onCycle: () => void }) {
  const label = themeLabel(pref)
  return (
    <button
      type="button"
      className="theme-toggle"
      onClick={onCycle}
      // 标签只说现状，动作只能靠 title / aria-label 补。
      title={`主题：${label} · 点击切换`}
      aria-label={`主题：${label}，点击切换`}
    >
      <span className="theme-toggle__icon" aria-hidden="true">
        {ICONS[pref]}
      </span>
      <span className="theme-toggle__label">{label}</span>
    </button>
  )
}
