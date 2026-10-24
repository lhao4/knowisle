//! 知屿 KnowIsle · Tauri 适配器。
//!
//! 这一层应当薄到无聊：注册插件、注册命令、启动窗口。
//! 所有真正的逻辑都在 `ki-core` / `ki-vault` 里，因此这里没有业务代码，
//! 也就不需要为它写单元测试——它唯一的验收标准是"能起来"。

mod commands;

use commands::{app_ping, note_read, vault_scan};
use tauri_specta::{Builder, ErrorHandlingMode};

/// 构建 IPC 契约。
///
/// 这是命令注册的**唯一入口**，两个消费者共用它：
///
/// - [`run`] 拿 `invoke_handler()`，决定运行时哪些命令真的存在；
/// - `src/bin/export_bindings.rs` 拿 `export()`，决定 `bindings.ts` 里有哪些函数。
///
/// 分成两处写就会出现"前端能编译、运行时 404"的组合——所以宁可多一个函数。
pub fn ipc_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .commands(tauri_specta::collect_commands![
            vault_scan, note_read, app_ping
        ])
        // 默认是 `Result` 模式，生成 `{ status: "ok" | "error", ... }` 包装。
        // 那会逼着前端每处调用都改写一遍——这是行为变更，不属于"换契约"的范围。
        // `Throw` 让生成的函数直接返回 `Promise<T>` 并在失败时 reject，
        // 与现有 `invoke()` 的语义逐字相同，`try/catch` 一行都不用动。
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let ipc = ipc_builder();

    tauri::Builder::default()
        // 文件夹选择对话框。前端通过 @tauri-apps/plugin-dialog 调用，
        // 权限在 capabilities/default.json 里只放行 dialog:allow-open。
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(ipc.invoke_handler())
        .run(tauri::generate_context!())
        .expect("error while building tauri application");
}
