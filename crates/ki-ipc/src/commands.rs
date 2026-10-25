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

use ki_core::{AppInfo, KiError, NoteContent, SaveResult, SaveState, ScanResult, SearchResult};
use ki_vault::{
    create_note, delete_note, read_note, rename_note, scan_vault, write_note, ScanOptions,
};

use crate::{autosave, search};

/// 扫描 Vault，返回 POSIX 相对路径的树。
///
/// 扫描同时做两件事：
///
/// 1. **丢掉内存索引缓存**：用户点「选择文件夹」就是在要求"重新看一眼磁盘"，
///    留着旧索引会让"外部改动重新扫描后才可见"这句话变成谎言。
/// 2. **把索引库对齐到磁盘**（`search::sync_after_scan`）：判据是 `size + mtime`，
///    一次没有任何变化的扫描读 0 字节正文。
///
/// # 为什么同步失败不让扫描失败
///
/// 用户点的是「选择文件夹」，要的是树。索引维护失败时最坏结果是"搜不到新内容"，
/// 而那会在下一次扫描时自愈；让整次扫描报错则是"库都打不开了"。
/// 失败只写 stderr——用户真去搜索时，`KiError::Index` 会把具体路径带出来。
#[tauri::command]
#[specta::specta]
pub async fn vault_scan(root: String) -> Result<ScanResult, KiError> {
    run_blocking(move || {
        let root = PathBuf::from(root);
        search::invalidate(&root);
        let scan = scan_vault(&root, &ScanOptions::default())?;
        if let Err(err) = search::sync_after_scan(&root, &scan) {
            eprintln!("索引同步失败（不影响本次扫描）：{err}");
        }
        Ok(scan)
    })
    .await
}

/// 全文搜索：ASCII 大小写折叠后的子串匹配。
///
/// # 参数
///
/// - `query` 是**子串**，不是正则、不分词。首尾空白忽略。
/// - `refresh_path` 是"当前正在编辑的笔记"：搜索前把它从磁盘重读一次，
///   让刚打的字立刻可见。其余文件不重读（那叫重建索引）。
///
/// # 为什么这里不 flush
///
/// "先落盘再搜"是**调用方**的顺序（前端先 `note_flush` 再调本命令）。
/// 查询若在这里偷偷写盘，一次只读操作就变成了写操作，而写盘失败会连带
/// 让搜索报错——用户会以为"搜不了"。
#[tauri::command]
#[specta::specta]
pub async fn vault_search(
    root: String,
    query: String,
    refresh_path: Option<String>,
) -> Result<SearchResult, KiError> {
    run_blocking(move || search::query(Path::new(&root), &query, refresh_path.as_deref())).await
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

/// 新建一篇**空**笔记，返回它的 Vault 相对路径。
///
/// # 参数
///
/// - `dir` 是 Vault 相对目录（POSIX 分隔符，空串表示根目录），**必须已存在**。
///   不代建目录：目录是另一个功能（M2 明确不做），悄悄造一个只会让树里
///   多出一串用户没要求的层级。
/// - `name` 是单个文件名。没有 `.md` / `.markdown` 后缀会自动补 `.md`；
///   合法性判定在 `ki_vault::naming`——那是唯一闸门，前端只做即时反馈。
///
/// # 为什么是 0 字节
///
/// 不写标题、不写模板。用户没写的字，编辑器不该替他写。
///
/// # 为什么不用原子写
///
/// 新建没有"旧内容"可保护，而 `create_new` 能在内核层面原子地拒绝撞名——
/// 这恰恰是原子写给不了的。撞名返回 [`KiError::NameTaken`]，原文件不动。
#[tauri::command]
#[specta::specta]
pub async fn note_create(root: String, dir: String, name: String) -> Result<String, KiError> {
    run_blocking(move || create_note(&PathBuf::from(root), &dir, &name)).await
}

/// 重命名一篇笔记（**同目录内改名**，不移动、不删除）。
///
/// # 为什么先 drain 再改名
///
/// 自动保存的 worker 可能正在写这篇笔记。若先改名，worker 会把内容写到
/// 已经不存在的旧路径上——旧文件"复活"，而新文件停在旧内容。所以顺序是
/// **先落盘并等 worker 退出 → 再 rename → 最后把槽位键搬到新路径**，
/// 三步都在 Rust 侧完成（见 `autosave::drain_for_rename` / `rename_slot`）。
///
/// 前端先 flush 一遍只是为了让编辑器 300ms 合并窗口里的字也参与进来，
/// **不能替代**这里的 drain：前端 flush 返回时，worker 可能还没退。
#[tauri::command]
#[specta::specta]
pub async fn note_rename(root: String, path: String, name: String) -> Result<String, KiError> {
    run_blocking(move || {
        let root = PathBuf::from(root);
        autosave::drain_for_rename(&root, &path)?;
        let next = rename_note(&root, &path, &name)?;
        autosave::rename_slot(&root, &path, &next);
        Ok(next)
    })
    .await
}

/// 删除一篇笔记：**移进 `.knowisle/trash/`**，不抹掉文件。
///
/// 返回它在回收目录里的 Vault 相对路径（`.knowisle/trash/<unix_ms>-<原名>`）。
///
/// # 顺序：drain → trash → clear_slot
///
/// 三步都在 Rust 侧、都在同一个阻塞任务里，任何一步失败都会让后面两步不执行：
///
/// 1. **drain**：槽位里可能压着用户最后 1.5 秒的输入。不先落盘就搬目录项，
///    trash 里那份就是**旧版本**——删除变成了"悄悄回退一个版本再删"。
///    冲突时返回 `Err`，删除直接拒绝：此刻用户还有「覆盖 / 放弃」两个选择。
/// 2. **trash**：一次 `fs::rename`，同盘原子。失败时文件仍在原位，状态一致，
///    重试即可——**不要**在失败路径里清理 trash，那只会多一次删文件的机会。
/// 3. **clear_slot**：把槽位清空。这一步不能省：前端 `discardPending` 只挡住
///    300ms 合并窗口，挡不住已经在飞的 `noteFlush`——它落在已删除路径上会
///    把文件写回来。`key = None` 之后任何迟到的 flush 都是无操作。
///
/// # 为什么不能先让前端 `setActiveNote(null)`
///
/// 编辑器一卸载，`getContent()` 就拿不到内容了，第 1 步会往 trash 写一个
/// 旧版本。所以"离开这篇文档"的动作必须排在 drain 之后——这是本轮唯一新增的
/// **离开**路径，它和打开路径一样不能绕开既有的三条规矩。
///
/// # 拒绝的输入
///
/// - 目录（`fs::rename` 对目录合法，一次能搬走一整片笔记）
/// - `.knowisle/` 下的任何路径（否则 `note_delete(".knowisle/index.sqlite")`
///   能删掉索引，`note_delete(".knowisle/trash/x.md")` 会把回收目录里那份再删一次）
/// - `..` / 绝对路径（`PATH_ESCAPE`）
#[tauri::command]
#[specta::specta]
pub async fn note_delete(root: String, path: String) -> Result<String, KiError> {
    run_blocking(move || {
        let root = PathBuf::from(root);
        autosave::drain_for_delete(&root, &path)?;
        let trashed = delete_note(&root, &path)?;
        autosave::clear_for_delete(&root, &path);
        Ok(trashed)
    })
    .await
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
