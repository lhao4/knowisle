//! Tauri 命令层。
//!
//! 本层的唯一职责是「解包参数 → 调 service → 序列化结果」。
//! 每个命令都应短到无聊；一旦这里出现业务逻辑，说明它跑错了层
//! （架构文档 2.3：每个 command ≤ 15 行）。
//!
//! # 关于 `#[specta::specta]`
//!
//! 每个命令都必须带这个标注，否则 `collect_commands!` 收不到它的类型信息，
//! 生成的 `bindings.ts` 里会**静默缺一条**——不会报错，只是前端没有这个函数。
//! 宏的顺序也不能反：`#[tauri::command]` 在上，`#[specta::specta]` 在下。

use std::path::PathBuf;

use ki_core::{AppInfo, KiError, NoteContent, SaveResult, ScanResult};
use ki_vault::{read_note, scan_vault, write_note, ScanOptions};

/// 扫描 Vault，返回 POSIX 相对路径的树。
#[tauri::command]
#[specta::specta]
pub async fn vault_scan(root: String) -> Result<ScanResult, KiError> {
    run_blocking(move || scan_vault(&PathBuf::from(root), &ScanOptions::default())).await
}

/// 读取单篇笔记。`path` 必须是 `vault_scan` 返回过的 POSIX 相对路径。
#[tauri::command]
#[specta::specta]
pub async fn note_read(root: String, path: String) -> Result<NoteContent, KiError> {
    run_blocking(move || read_note(&PathBuf::from(root), &path)).await
}

/// 原子地写入单篇笔记。
///
/// # 参数语义（前端必须知道的四条）
///
/// - `path` 是 **Vault 相对路径**（POSIX 分隔符），不是绝对路径。
/// - `content` 是**完整正文**，不是增量。本命令不做 diff、不做合并。
/// - **父目录必须已存在**：不会替用户建目录。要新建笔记，先确保目标目录在库里。
/// - 写入是原子的（`tmp → fsync → rename`），崩溃只会看到旧内容或新内容，
///   不会看到半截。
#[tauri::command]
#[specta::specta]
pub async fn note_save(root: String, path: String, content: String) -> Result<SaveResult, KiError> {
    run_blocking(move || write_note(&PathBuf::from(root), &path, &content)).await
}

/// 健康检查：前端启动时用它确认 IPC 通道是通的。
///
/// 返回 `AppInfo` 而不是 `serde_json::Value`：后者对 specta 是不可描述的，
/// 生成出来会是 `unknown`，等于没有契约。
#[tauri::command]
#[specta::specta]
pub fn app_ping() -> AppInfo {
    AppInfo {
        name: "knowisle".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
    }
}

/// 把阻塞型任务丢到专用线程池。
///
/// 这不是可选优化：`async fn` 命令跑在 tokio 的核心线程上，
/// 一次 500ms 的目录扫描会阻塞其它命令与事件循环，表现为
/// "点按钮之后整个界面卡住"。现在多花 3 行，省掉 M3 的一次性能事故。
///
/// 写入同理，而且更严重：`fsync` 是**磁盘级同步**，在机械盘或被杀毒软件
/// 拦截的目录里可以卡几百毫秒。它必须离开 IPC 线程。
async fn run_blocking<T, F>(task: F) -> Result<T, KiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, KiError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|err| KiError::Internal(format!("后台任务失败：{err}")))?
}
