//! 知屿 KnowIsle · Tauri 命令层
//!
//! 本 crate 是 `src-tauri` 与领域层之间的那一层：**解包参数 → 调 service → 序列化结果**。
//! 每个命令都应短到无聊；一旦这里出现业务逻辑，说明它跑错了层。
//!
//! # 为什么不直接放在 `src-tauri` 里
//!
//! 因为 `src-tauri` 是一个二进制壳：它的测试要链接 webview，它的 clippy 要系统 GUI 库，
//! 于是"命令层不进 CI 的 lint，也写不了测试"——`note_save` 这种会丢数据的命令，
//! 恰恰最需要被验证。把命令层挪出来之后：
//!
//! - `tauri` 关掉 default features，不再拉 wry；
//! - `cargo test -p ki-ipc` 能直接调用命令函数（`#[tauri::command]` 不会吃掉原函数）；
//! - `cargo clippy -p ki-ipc -D warnings` 终于有地方跑。
//!
//! `src-tauri` 剩下的唯一职责是"起窗口"，薄到不需要测试——它也确实没有测试。
//!
//! # 命名约定
//!
//! 架构文档原本写 `domain::action`（如 `note::save`），但 Tauri 命令名必须是
//! 合法的 Rust 标识符，`::` 不可用。因此统一为 `domain_action`：
//! `vault_scan`、`note_read`、`note_save`、`app_ping`。
//!
//! 生成到 TS 后访问器是小驼峰（`commands.noteSave`），但 IPC 字符串仍是下划线。

mod commands;

pub use commands::{app_ping, note_read, note_save, vault_scan};

use tauri_specta::{Builder, ErrorHandlingMode};

/// 构建 IPC 契约。
///
/// 这是命令注册的**唯一入口**，两个消费者共用它：
///
/// - 应用壳拿 `invoke_handler()`，决定运行时哪些命令真的存在；
/// - `src-tauri/src/bin/export_bindings.rs` 拿 `export()`，决定 `bindings.ts` 里有哪些函数。
///
/// 分成两处写就会出现"前端能编译、运行时 404"的组合——所以宁可多一个函数。
///
/// 泛型而不是写死 `tauri::Wry`：本 crate 不依赖 wry，运行时由调用方决定。
pub fn ipc_builder<R: tauri::Runtime>() -> Builder<R> {
    Builder::<R>::new()
        .commands(tauri_specta::collect_commands![
            vault_scan, note_read, note_save, app_ping
        ])
        // 默认是 `Result` 模式，生成 `{ status: "ok" | "error", ... }` 包装。
        // 那会逼着前端每处调用都改写一遍——这是行为变更，不属于"换契约"的范围。
        // `Throw` 让生成的函数直接返回 `Promise<T>` 并在失败时 reject，
        // 与 `invoke()` 的原始语义逐字相同，`try/catch` 一行都不用动。
        .error_handling(ErrorHandlingMode::Throw)
        // `u64` / `i64` 默认会被导成 TS `bigint`，而 bigint 默认是**禁止导出**的（会直接报错）。
        // 但 Tauri v2 的 IPC 走 JSON，`u64` 到了 webview 里本来就是 double——
        // 导成 bigint 反而是错的。详见 `ki-core/src/node.rs` 的"数值宽度"。
        .dangerously_cast_bigints_to_number()
        // 默认会为"序列化形状 ≠ 反序列化形状"的类型各生成一份 `_Serialize` / `_Deserialize`。
        // 触发条件往往只是一个 `skip_serializing_if`，代价却是前端拿到
        // `FileNode = FileNode_Serialize | FileNode_Deserialize` 这种联合类型，
        // 访问 `node.children` 就开始出现联合数组，报错信息极难读。
        // 本项目所有类型形状对称，统一成一份即可。
        .disable_serde_phases()
}
