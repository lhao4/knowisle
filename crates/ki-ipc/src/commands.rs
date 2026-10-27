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

use ki_core::{
    AppInfo, KiError, NoteContent, PurgeResult, RestoreResult, SaveResult, SaveState, ScanResult,
    SearchResult, TrashCursor, TrashPage,
};
use ki_vault::trash as trash_ops;
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
/// 2. **把索引库对齐到磁盘，但不等它**（`search::spawn_after_scan`）：
///    判据仍是 `size + mtime`，一次没有任何变化的扫描读 0 字节正文。
///
/// # 为什么索引改到后台
///
/// 10k 篇首次建库要 10 秒（缓存热）到 108 秒（缓存冷，本机实测）。用户点的是
/// 「选择文件夹」，要的是那棵树；让他等索引建完，等于把「打开库」变成
/// 「等两分钟」。索引是搜索的派生数据，可以晚一点到。
///
/// # 代价：搜索可能不全，所以它**可见**
///
/// 本命令返回之前就把索引状态置成 `building`，之后每次 `vault_search` 都带着
/// `indexState` 回来（`building { done, total }` / `ready` / `failed`）。
/// 前端据此显示「索引正在建立（N/M），以下结果可能不全」。
///
/// 没有这个可见性，后台索引就是一个静默的不一致：搜不到刚扫描进来的文件，
/// 而界面上没有任何东西说明"还在建"。
///
/// # 失败不报错
///
/// 树已经返回了，索引维护失败最多让搜索少几条，下次扫描会重试。失败只写
/// stderr 并反映在 `indexState` 的 `failed` 上——用户真去搜索时，
/// `KiError::Index` 会把具体路径带出来。
#[tauri::command]
#[specta::specta]
pub async fn vault_scan(root: String) -> Result<ScanResult, KiError> {
    run_blocking(move || {
        let root = PathBuf::from(root);
        search::invalidate(&root);
        let scan = scan_vault(&root, &ScanOptions::default())?;
        search::spawn_after_scan(&root, &scan);
        Ok(scan)
    })
    .await
}

/// 全文搜索：ASCII 大小写折叠后的字面子串匹配，多词取 AND。
///
/// # 参数
///
/// - `query` 是**字面子串**，不是正则、不做统计分词。按空白拆成多个词时取
///   AND（每个词都得出现在同一行里）；首尾空白忽略。
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

/// 列出回收站的一页，按删除时间倒序。
///
/// # 三个字段的含义不能混
///
/// - `entries` 是**这一页**；`total` 是**一共有多少条**。加载两页之后
///   `entries.length` 是 400，而 `total` 可能是 10000——两个问题，两个答案。
/// - `next` 是**事实**：`null` 表示游标后面确实没有别的条目了。前端必须
///   原样传回下一次调用，不要拿 `entries.length < limit` 自己推。
/// - `entry` 是**回收目录里的真名**，恢复时原样传回 `trash_restore`；
///   `origin` 是删除前的相对路径，`null` 表示清单缺失/损坏。**这不是错误**：
///   文件还在，只是不知道它原来在哪。UI 必须显示成「原路径未知」。
///
/// # 参数
///
/// - `cursor` 首页传 `null`，之后传上一页的 `next`。keyset 而不是 offset：
///   中间恢复一条不会让后面的行移位。
/// - `limit` 服务端钳制（`0 → 200`、`> 500 → 500`）。前端传什么都越不过它，
///   否则一次调用就能读 10 万条清单，分页等于没做。
///
/// 列表为空是正常状态（从没删过东西），不返回错误。
#[tauri::command]
#[specta::specta]
pub async fn trash_list(
    root: String,
    cursor: Option<TrashCursor>,
    limit: u32,
) -> Result<TrashPage, KiError> {
    run_blocking(move || trash_ops::page(&PathBuf::from(root), cursor.as_ref(), limit)).await
}

/// 把回收站里的一条搬回原目录，返回它最终落在哪里。
///
/// # 返回值里三个字段各说一件事（前端按它们措辞）
///
/// | 字段 | 说什么 |
/// |---|---|
/// | `path` / `requested` | 事实 / 意图。降级时两者分叉（`c.md` vs `a/b/c.md`） |
/// | `fallback` | 为什么没回原处：`recordMissing` / `recordCorrupt` / `originGone` |
/// | `renamed` | 有没有**真的**撞名改名（与 `fallback` 正交，可同时发生） |
/// | `indexed` | 索引里有没有这一行 |
///
/// # 为什么要在这里补索引
///
/// 恢复之后文件回到磁盘，但索引里还没有它。前端恢复完会重扫一次（要刷新树），
/// 可"搜索能找到"这件事不该依赖调用方记得重扫——将来的 `isle` CLI 不会。
/// 所以这里定点补一行。
///
/// **补索引失败不改命令的成功语义**：文件已经在正确的位置了，判成失败会诱导
/// 用户重试（而重试会得到 `c 2.md`）。只把事实带出去：`indexed = false`，
/// 由前端说「索引未更新，重启应用会重建」。索引库可整库重建（ADR-004），
/// 所以那句话是真的。
#[tauri::command]
#[specta::specta]
pub async fn trash_restore(root: String, name: String) -> Result<RestoreResult, KiError> {
    run_blocking(move || {
        let root = PathBuf::from(root);
        let mut restored = trash_ops::restore(&root, &name)?;
        match search::reindex(&root, &restored.path) {
            Ok(()) => restored.indexed = true,
            Err(err) => eprintln!("恢复后的索引更新失败（重启应用会重建）：{err}"),
        }
        Ok(restored)
    })
    .await
}

/// 清空回收站，返回「删掉了几个、还剩几个」。
///
/// 只删文件与**对应的清单记录**，不递归删用户自己放进来的子目录（那不是
/// 我们造的东西）。删不掉那几篇的记录会留着——文件还在回收目录里，原路径
/// 不能丢，否则列表上那一条会退化成「原路径未知」。
/// 部分文件删不掉时，其余照删，`failed` 计数——**不返回错误**。用户点"清空"
/// 的意图是"能删的都删掉"，而报错会让前端既不刷新列表也不说删了几个，
/// 面板上于是还列着已经被删掉的文件。
///
/// 唯一会 `Err` 的情况是回收目录本身读不出来：那时一个都没动，是真的失败。
#[tauri::command]
#[specta::specta]
pub async fn trash_purge(root: String) -> Result<PurgeResult, KiError> {
    run_blocking(move || trash_ops::purge(&PathBuf::from(root))).await
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
