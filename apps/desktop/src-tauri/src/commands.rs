//! Tauri 命令层。
//!
//! 本层的唯一职责是「解包参数 → 调 service → 序列化结果」。
//! 每个命令都应短到无聊；一旦这里出现业务逻辑，说明它跑错了层
//! （架构文档 2.3：`src-tauri` 里每个 command ≤ 15 行）。
//!
//! # 命名约定
//!
//! 架构文档原本写 `domain::action`（如 `note::save`），但 Tauri 命令名必须是
//! 合法的 Rust 标识符，`::` 不可用。因此统一为 `domain_action`：
//! `vault_scan`、`note_read`、`app_ping`。

use std::path::PathBuf;

use ki_core::{KiError, NoteContent, ScanResult};
use ki_vault::{ScanOptions, read_note, scan_vault};

/// 扫描 Vault，返回 POSIX 相对路径的树。
#[tauri::command]
pub async fn vault_scan(root: String) -> Result<ScanResult, KiError> {
    run_blocking(move || scan_vault(&PathBuf::from(root), &ScanOptions::default())).await
}

/// 读取单篇笔记。`path` 必须是 `vault_scan` 返回过的 POSIX 相对路径。
#[tauri::command]
pub async fn note_read(root: String, path: String) -> Result<NoteContent, KiError> {
    run_blocking(move || read_note(&PathBuf::from(root), &path)).await
}

/// 健康检查：前端启动时用它确认 IPC 通道是通的。
#[tauri::command]
pub fn app_ping() -> serde_json::Value {
    serde_json::json!({
        "name": "knowisle",
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
    })
}

/// 把阻塞型任务丢到专用线程池。
///
/// 这不是可选优化：`async fn` 命令跑在 tokio 的核心线程上，
/// 一次 500ms 的目录扫描会阻塞其它命令与事件循环，表现为
/// "点按钮之后整个界面卡住"。现在多花 3 行，省掉 M3 的一次性能事故。
async fn run_blocking<T, F>(task: F) -> Result<T, KiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, KiError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|err| KiError::Internal(format!("后台任务失败：{err}")))?
}
