# 知屿 KnowIsle

> 把知识，安放在自己的岛上。

本地优先、AI 驱动的开源 Markdown 个人知识库。所有笔记都是磁盘上普通的 `.md` 文件，索引与数据库只是可以随时重建的派生物 —— 删掉应用，你的知识还在原地。

当前处于 **M0（骨架）** 阶段：Vault 扫描、文件树、只读渲染链路已打通，检索与 AI 能力尚未开始。

## 技术栈

| 层 | 选型 |
|---|---|
| 外壳 | Tauri 2（Rust core + 系统 WebView） |
| 前端 | React 19 + TypeScript + Vite 8 + TanStack Router |
| 编辑器 | CodeMirror 6（唯一主编辑内核） |
| 存储 | SQLite + rusqlite（bundled）+ refinery 迁移，手写 Repository |
| 检索 | jieba-rs 预分词 + FTS5 双通道 + RRF 融合 |
| 样式 | Tailwind v4 + 三层 CSS 变量 token |

架构决策的完整依据见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)，它是本项目的唯一真相源；改动架构前先写 ADR，再改文档。

## 目录结构

```
crates/          # Rust 工作区：ki-core / ki-vault / ki-store / ki-search ...
apps/desktop/    # Tauri 应用：src-tauri（Rust 外壳）+ src（React 前端）
packages/        # 共享 TS 包：ui / editor / sdk / bindings
docs/            # 架构与范围文档
```

## 本地环境要求

| 依赖 | 版本 | 说明 |
|---|---|---|
| Node.js | ≥ 22 | `package.json` 的 `engines` 已锁定 |
| pnpm | 12.15.0 | 由 `packageManager` 字段固定，建议用 corepack 启用 |
| Rust | stable (msvc) | Windows 下必须是 `x86_64-pc-windows-msvc` 工具链 |
| WebView2 | 系统自带 | Windows 10/11 默认已安装 |

### 两个必须保留的配置（踩过的坑）

**1. pnpm 必须使用 `nodeLinker: hoisted`**

见 [`pnpm-workspace.yaml`](pnpm-workspace.yaml)。Windows 下创建目录符号链接需要开发者模式或管理员权限，缺权限时 pnpm 的默认隔离布局会**静默失败**——安装看似成功，运行时却找不到模块。`hoisted` 用真实目录代替 symlink，代价是失去 pnpm 的严格依赖隔离，换来「在任何 Windows 机器上 `pnpm install` 都不会静默失败」。

**2. Vite 必须显式绑定 `server.host: '127.0.0.1'`**

见 [`apps/desktop/vite.config.ts`](apps/desktop/vite.config.ts)。Node 18+ 的 `localhost` 在 Windows 上会优先解析到 `::1`，于是 Vite 只监听 `[::1]:1420`；而 WebView2 解析 `localhost` 时优先走 `127.0.0.1`，结果就是**应用窗口一片空白、控制台没有任何报错**。`tauri.conf.json` 的 `devUrl` 因此也写死为 `http://127.0.0.1:1420`，两处必须一致。

## 快速开始

```bash
corepack enable
pnpm install
pnpm dev          # 启动 Tauri 开发窗口（内部会拉起 Vite）
```

常用校验：

```bash
pnpm typecheck    # tsc -b，全工作区
pnpm lint         # oxlint
pnpm check        # cargo check --workspace
pnpm test         # cargo test --workspace
```

## 许可证

[GNU AGPL-3.0](LICENSE)。你可以自由使用、修改、分发，但如果把它作为网络服务提供给别人用，必须同样开源你的修改。
