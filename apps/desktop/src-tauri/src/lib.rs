//! 知屿 KnowIsle · Tauri 应用壳。
//!
//! 这一层现在只剩三件事：**注册插件**、**起窗口**、**关窗前落盘**。
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
        // 关窗前把 Rust 槽位里的内容落盘。
        //
        // # 为什么是同步的
        //
        // 这个回调返回之后 Tauri 才会销毁窗口、退出事件循环。闭包里不能
        // `await`，也不能把工作丢给别的线程然后立刻返回——那样进程会在写盘
        // 完成前就没了。`flush_slot()` 是同步函数，就是为了能在这里直接调。
        //
        // # 为什么不做 `prevent_close` + 前端往返
        //
        // 那条路要「先拦住窗口 → 通知前端 → 前端把 300ms 缓冲发上来 → 再关」。
        // 它多出三个失效模式：前端没响应（窗口就永远关不掉）、事件与 destroy
        // 权限配错（拦住了但放不开）、以及一个必须自己实现的看门狗。
        // 换来的只是把「最多丢 300ms」压到 0。
        //
        // # 代价（写在这里，因为这里就是它的边界）
        //
        // Rust 槽位里的内容关窗时落盘；但前端 300ms 缓冲里的字符仍会丢——
        // 本轮接受。把「最多丢 300ms」压到 0 需要「事件 + destroy 权限 +
        // 看门狗」，不值。
        .on_window_event(|_window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                ki_ipc::autosave::flush_slot();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while building tauri application");
}
