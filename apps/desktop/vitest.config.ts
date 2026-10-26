import { defineConfig } from 'vitest/config'

/**
 * 纯逻辑用例的运行配置。
 *
 * # 为什么单独一份，不复用 `vite.config.ts`
 *
 * 这里测的是 `src/lib/*` 与 `src/api/errors.ts`——**不测组件**，所以不需要
 * react 插件，也不需要 DOM。让 vitest 去继承 vite 的配置，只会多一次插件加载
 * （@vitejs/plugin-react 要在启动时做 babel/oxc 的初始化），换不来任何东西。
 *
 * `environment: 'node'` 是硬约束，不是省事：它让「误引 `@codemirror/view`」
 * 立刻炸成 `document is not defined`，而不是在 jsdom 里静默通过。
 * `outline.ts` 只该用 `@codemirror/state` 的纯 state 层，这条配置就是那道闸。
 */
export default defineConfig({
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
    // 提交里残留一个 `.only`，其余用例会静默不跑——CI 照样绿，覆盖归零。
    // 让它在跑之前直接失败，比事后 review 可靠。
    allowOnly: false,
  },
})
