//! 知屿 KnowIsle · Tauri 应用壳。
//!
//! 这一层现在只剩两件事：**注册插件**、**起窗口**。
//! 命令层在 `ki-ipc`，业务逻辑在 `ki-vault` / `ki-core`。
//!
//! 把命令层挪走不是洁癖：`src-tauri` 的测试要链接 webview、clippy 要 GUI 系统库，
//! 于是它天然进不了 CI 的 lint 与测试——而会丢数据的 `note_save` 恰恰最需要被验证。
//! 现在这里薄到不需要测试，它也确实没有测试。

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let ipc = ki_ipc::ipc_builder::<tauri::Wry>();

    tauri::Builder::default()
        // 文件夹选择对话框。前端通过 @tauri-apps/plugin-dialog 调用，
        // 权限在 capabilities/default.json 里只放行 dialog:allow-open。
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(ipc.invoke_handler())
        .run(tauri::generate_context!())
        .expect("error while building tauri application");
}
