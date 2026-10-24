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

use std::path::{Path, PathBuf};

use ki_core::{AppInfo, KiError, NoteContent, SaveResult, SaveState, ScanResult};
use ki_vault::{read_note, scan_vault, write_note, ScanOptions};

use crate::autosave;

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

/// 提交一次编辑，由 Rust 侧的节流器决定何时落盘。
///
/// # 为什么不是前端定时调 `note_save`
///
/// 前端定时器会被 webview 节流（窗口失焦时降到约 1 次/分钟），
/// 用户切去别的应用再回来，中间那段时间根本没保存。放在这里则不受影响。
///
/// # 参数
///
/// - `content` 是**完整正文**，不是增量。
/// - `base_mtime` 是打开这篇笔记时 `note_read` 返回的 mtime，
///   **只在切换文档时采信一次**。同一文档的后续调用可以一直传同一个值，
///   基线由 Rust 侧随每次成功落盘向前推进。
///
/// # 返回值
///
/// [`SaveState`]：是否有内容未落盘、最近一次落盘的 mtime、最近一次错误。
/// 前端据此渲染「已保存 / 未保存 / 冲突」。
#[tauri::command]
#[specta::specta]
pub async fn note_edit(
    root: String,
    path: String,
    content: String,
    base_mtime: Option<i64>,
) -> Result<SaveState, KiError> {
    run_blocking(move || {
        Ok(autosave::submit(
            PathBuf::from(root),
            path,
            content,
            base_mtime,
        ))
    })
    .await
}

/// 立刻落盘（切换笔记、关窗前调用），不必等节流器的下一个周期。
///
/// `force` 为真时跳过乐观锁，直接覆盖——对应冲突提示里的「用我的版本覆盖」。
///
/// 返回 `None` 表示本来就没有待落盘的内容。
#[tauri::command]
#[specta::specta]
pub async fn note_flush(
    root: String,
    path: String,
    force: bool,
) -> Result<Option<SaveResult>, KiError> {
    run_blocking(move || autosave::flush(Path::new(&root), &path, force)).await
}

/// 查询保存状态，不触发写入。
///
/// 自动落盘在 Rust 侧异步发生，前端只能靠它知道「什么时候存完了」。
/// 只在前端 `dirty` 为真时轮询，空闲时不轮询。
#[tauri::command]
#[specta::specta]
pub async fn note_status(root: String, path: String) -> Result<SaveState, KiError> {
    Ok(autosave::status(Path::new(&root), &path))
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
