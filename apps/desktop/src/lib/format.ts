/** 纯展示层的格式化工具。与 IPC 无关，所以不在 `api/` 里。 */

/** 单位表。以 1024 进位，与 Windows 资源管理器一致。 */
const BYTE_UNITS = ['B', 'KB', 'MB', 'GB', 'TB'] as const

/**
 * 一位小数规则：**≥10 不留小数，<10 留一位**。
 *
 * 这是这个函数一开始就有的取整习惯（`2.0 MB` / `512 KB` / `1.5 KB`），
 * 回收站的每一行与 `TOO_LARGE` 的错误文案都在用它。修下面的进位 bug 时
 * **刻意没有顺手改它**——那是一次与本次改动无关的可见变化。
 */
function trim(value: number): string {
  return value.toFixed(value >= 10 ? 0 : 1)
}

/**
 * 把字节数格式化成 `1.5 MB` / `0 B` / `1023 B`。
 *
 * # 修掉的那个 bug
 *
 * 旧实现先除、后 `toFixed(0)`，于是 `1048575` 字节会印成 **`1024 KB`**：
 * 四舍五入发生在进位判断之后，而它已经没有机会再进一位了。回收站的每一行
 * 都走这个函数，所以这个数是真的会出现在用户眼前的。
 *
 * 现在先取整、再判断满不满 1024，满了就再进一位。
 *
 * # 另外两个刻意的选择
 *
 * - **小于 1024 只报字节、不带小数**：字节没有小数，`512.0 B` 是假精度。
 * - 负数 / `NaN` / `Infinity` 一律当 0：这些值只可能来自算错的地方，而界面上
 *   显示 `-1 B` 比显示 `0 B` 更容易被当成真事。
 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'

  let value = Math.floor(bytes)
  let unit = 0
  while (value >= 1024 && unit < BYTE_UNITS.length - 1) {
    value /= 1024
    unit += 1
  }
  // 字节不带小数：`Math.floor` 之后原样输出。
  if (unit === 0) return `${value} B`

  const text = trim(value)
  if (Number(text) >= 1024 && unit < BYTE_UNITS.length - 1) {
    // 1023.96 KB → "1024" → 进一位 → 1.0 MB。少这一步就会印出 `1024 KB`。
    return `${trim(Number(text) / 1024)} ${BYTE_UNITS[unit + 1]}`
  }
  return `${text} ${BYTE_UNITS[unit]}`
}
