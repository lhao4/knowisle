import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// Tauri 要求端口固定：devUrl 写死在 tauri.conf.json 里，
// 端口被占用时宁可启动失败，也不要静默换端口导致白屏。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    // 必须显式绑 IPv4。
    //
    // Node 18+ 的 `localhost` 在 Windows 上会优先解析到 ::1，于是 Vite 只监听
    // [::1]:1420；而 WebView2 解析 localhost 时优先走 127.0.0.1，结果就是
    // 应用窗口一片空白、控制台没有任何报错。这个坑排查起来能耗掉半天。
    host: '127.0.0.1',
    port: 1420,
    strictPort: true,
    // src-tauri 里的 Rust 源码由 tauri dev 自己监听，
    // Vite 再监听一遍只会造成无意义的整页刷新。
    watch: { ignored: ['**/src-tauri/**'] },
  },
  build: {
    target: process.env.TAURI_ENV_PLATFORM === 'windows' ? 'chrome105' : 'safari13',
    // 不指定 'esbuild'：Vite 8 默认用 rolldown 自带的 oxc 压缩器，
    // 显式写 esbuild 反而会要求一个没装的依赖。
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: Boolean(process.env.TAURI_ENV_DEBUG),
  },
})
