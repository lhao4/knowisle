//! 知屿 KnowIsle · Tauri 适配器。
//!
//! 这一层应当薄到无聊：注册插件、注册命令、启动窗口。
//! 所有真正的逻辑都在 `ki-core` / `ki-vault` 里，因此这里没有业务代码，
//! 也就不需要为它写单元测试——它唯一的验收标准是"能起来"。

mod commands;

use commands::{app_ping, note_read, vault_scan};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 文件夹选择对话框。前端通过 @tauri-apps/plugin-dialog 调用，
        // 权限在 capabilities/default.json 里只放行 dialog:allow-open。
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![vault_scan, note_read, app_ping])
        .run(tauri::generate_context!())
        .expect("error while building tauri application");
}
