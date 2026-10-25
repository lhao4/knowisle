/**
 * 「这个用户被问过没有」。
 *
 * 与 `knowisle.lastVault` 是**两件事**，不许合并成一个 key：那个 key 记录的是
 * 「上次打开的库是哪个」，扫描失败时 `App` 会把它删掉（目录被删、改名、U 盘
 * 拔了）。于是「`lastVault === null`」既可能是首次启动，也可能是「上次的库
 * 扫不到了」——这两种情况该给的界面完全不同。
 */
export const ONBOARDED_KEY = 'knowisle.onboarded'

/**
 * 启动时要不要自动弹文件夹选择器。
 *
 * 判据只有一条：没有上次的库 **且** 没被问过。
 *
 * # 真值表
 *
 * | lastVault | onboarded | 自动弹 |
 * |-----------|-----------|--------|
 * | null      | null      | 是     |
 * | null      | '1'       | 否     |
 * | '/v'      | null      | 否     |
 * | '/v'      | '1'       | 否     |
 *
 * 只有第一行返回 `true`。第二行就是「上次的库扫不到了」那批人：他们已经
 * 见过一次对话框，不该每次启动都被弹。
 *
 * # 为什么第二个条件不能省
 *
 * 少了它，本机测不出任何问题（库一直在，`lastVault` 一直非空），只有真把
 * 目录删掉或拔掉 U 盘的用户才会遇到「每次启动都被弹窗」。这类 bug 在开发机
 * 上永远复现不了，所以判据必须写全。
 */
export function shouldAutoPick(input: {
  lastVault: string | null
  onboarded: string | null
}): boolean {
  return input.lastVault === null && input.onboarded !== '1'
}
