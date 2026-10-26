//! 回收站三个命令的端到端行为。
//!
//! 这里验的不是磁盘细节（那在 `ki-vault/tests/trash.rs`），而是**命令层把
//! 五件事接对了没有**：
//!
//! 1. 删除 → 列表 → 恢复 的路径往返；
//! 2. 恢复之后**不重扫也能搜到**（`trash_restore` 自己补了索引）；
//! 3. 撞名不覆盖、清空、以及条目被外部删掉时的错误码；
//! 4. 索引补不进去时，命令**仍然成功**，只把 `indexed = false` 带出去；
//! 5. 清空的部分失败用 `removed` / `failed` 表达，而不是返回错误。
//!
//! # 为什么必须串行
//!
//! `ki_ipc::search` 的缓存是进程全局的（见 `tests/search_fts.rs` 的文件头）。
//! 每个用例用自己的临时 Vault，串行之后 `builds` / `index_ms` 才是确定值。

use std::fs;
use std::sync::{Mutex, MutexGuard, PoisonError};

use ki_core::{IoKind, KiError};
use ki_ipc::{
    autosave, note_delete, note_read, trash_list, trash_purge, trash_restore, vault_scan,
    vault_search,
};
use tempfile::TempDir;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tauri::async_runtime::block_on(future)
}

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    autosave::reset_for_test();
    guard
}

fn root_of(vault: &TempDir) -> String {
    vault.path().to_string_lossy().into_owned()
}

fn scan(root: &str) {
    block_on(vault_scan(root.to_owned())).expect("扫描失败");
}

fn search(root: &str, query: &str) -> ki_core::SearchResult {
    block_on(vault_search(root.to_owned(), query.to_owned(), None)).expect("搜索失败")
}

/* ───────────────────── 往返 ───────────────────── */

/// 验收 1/2/6 的命令层：删 `a/b/c.md` → 列表有原路径 → 恢复 → 回到原处、
/// 内容一致、**不重扫**也能被搜到。
///
/// 「不重扫也能搜到」是这条用例的重点。前端恢复完确实会重扫一次（要刷新树），
/// 但把"搜索能找到"挂在前端的自觉上，等于给将来的 `isle` CLI 埋一个
/// 不报错的洞：文件回来了，搜不到，谁也不知道为什么。
#[test]
fn trash_commands_round_trip_and_make_the_note_searchable_again() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    let body = "回收站探针：正文里有独特标记\n";
    fs::write(vault.path().join("a/b/c.md"), body).unwrap();

    scan(&root);
    assert_eq!(
        search(&root, "回收站探针").hits.len(),
        1,
        "删除之前必须先搜得到，否则后面那条断言证明不了任何事"
    );

    let trashed = block_on(note_delete(root.clone(), "a/b/c.md".to_owned())).expect("删除失败");
    // 前端删完会重扫（树要更新），索引同步挂在它后面。这一步把索引里的行去掉。
    scan(&root);
    assert!(
        search(&root, "回收站探针").hits.is_empty(),
        "删除之后不该还能搜到"
    );

    let listed = block_on(trash_list(root.clone())).expect("列表失败");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].origin.as_deref(), Some("a/b/c.md"));
    assert_eq!(listed[0].entry, trashed.rsplit('/').next().unwrap());
    assert_eq!(listed[0].size, body.len() as u64);

    let restored =
        block_on(trash_restore(root.clone(), listed[0].entry.clone())).expect("恢复失败");
    assert_eq!(restored.path, "a/b/c.md");
    assert_eq!(restored.requested, "a/b/c.md");
    assert!(!restored.renamed);
    assert_eq!(restored.fallback, None, "原目录还在，不该有任何降级");
    assert!(restored.indexed, "索引库好好的，恢复必须把这一行补进去");

    // 关键：**没有**再 scan 一次。
    let hits = search(&root, "回收站探针").hits;
    assert_eq!(hits.len(), 1, "恢复后必须立刻能搜到，实际 {hits:?}");
    assert_eq!(hits[0].path, "a/b/c.md");

    let note = block_on(note_read(root.clone(), "a/b/c.md".to_owned())).expect("读回失败");
    assert_eq!(note.content, body);

    assert!(
        block_on(trash_list(root)).unwrap().is_empty(),
        "恢复之后列表必须空"
    );
}

/* ───────────────────── 撞名 ───────────────────── */

/// 验收 3 的命令层：同名已存在 → 不覆盖，改名为 `c 2.md`，两份都在。
#[test]
fn restoring_onto_an_existing_name_keeps_both_copies() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    fs::write(vault.path().join("c.md"), "删掉的那份").unwrap();

    scan(&root);
    block_on(note_delete(root.clone(), "c.md".to_owned())).expect("删除失败");
    fs::write(vault.path().join("c.md"), "新写的那份").unwrap();

    let listed = block_on(trash_list(root.clone())).expect("列表失败");
    let restored =
        block_on(trash_restore(root.clone(), listed[0].entry.clone())).expect("恢复失败");

    assert!(restored.renamed);
    assert_eq!(restored.requested, "c.md");
    assert_eq!(restored.path, "c 2.md");
    assert_eq!(
        fs::read_to_string(vault.path().join("c.md")).unwrap(),
        "新写的那份"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("c 2.md")).unwrap(),
        "删掉的那份"
    );
}

/* ───────────────────── 清空 ───────────────────── */

/// 验收 6 的命令层：清空返回条数，列表与磁盘都空。
#[test]
fn purging_empties_the_trash_and_the_list() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    for name in ["a.md", "b.md"] {
        fs::write(vault.path().join(name), name).unwrap();
        scan(&root);
        block_on(note_delete(root.clone(), name.to_owned())).expect("删除失败");
    }
    assert_eq!(block_on(trash_list(root.clone())).unwrap().len(), 2);

    let removed = block_on(trash_purge(root.clone())).expect("清空失败");

    assert_eq!(removed.removed, 2);
    assert_eq!(removed.failed, 0);
    assert!(block_on(trash_list(root.clone())).unwrap().is_empty());
    let left: Vec<String> = fs::read_dir(vault.path().join(".knowisle/trash"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.is_empty(), "回收目录必须空，实际 {left:?}");
}

/* ───────────────────── 索引补不进去 ───────────────────── */

/// 索引库坏了 → 恢复**仍然成功**，只把 `indexed = false` 带出去。
///
/// # 为什么"把索引库设成只读"造不出这个状态
///
/// 索引连接在 `vault_scan` 时就已经打开了，之后 `chmod` 只影响**新**句柄——
/// 已打开的那个照样写得进去（Windows 上连文件属性都不看）。要让 `reindex`
/// 真的失败，得让它在**执行 SQL 时**报错。这里直接把 `meta` 表删掉：
/// `reindex` 的第一步 `synced_at()` 就会失败，而索引库本身仍然是"能打开、
/// 能查询"的——这正是真实事故（库被别的进程占着、页结构损坏）的形态。
///
/// # 判成失败是错的
///
/// 文件已经在正确的位置了。判成 `Err` 会诱导用户重试，而重试会得到
/// `c 2.md`——一次索引故障变成两份笔记。前端该说的是「索引未更新，
/// 重启应用会重建」（索引库可整库重建，ADR-004，所以那句话是真的）。
#[test]
fn a_restore_that_cannot_update_the_index_still_succeeds() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let body = "索引注入探针：正文\n";
    fs::write(vault.path().join("a.md"), body).unwrap();
    scan(&root);

    let db = vault.path().join(".knowisle/index.sqlite");
    let conn = rusqlite::Connection::open(&db).expect("打开索引库");
    conn.execute_batch("DROP TABLE meta").expect("删掉 meta 表");
    drop(conn);

    let trashed = block_on(note_delete(root.clone(), "a.md".to_owned())).expect("删除失败");
    let name = trashed.rsplit('/').next().unwrap().to_owned();
    let restored = block_on(trash_restore(root.clone(), name)).expect("索引坏了不该让恢复失败");

    assert_eq!(restored.path, "a.md");
    assert_eq!(restored.fallback, None);
    assert!(!restored.indexed, "索引没更新，必须如实报告");
    assert_eq!(
        fs::read_to_string(vault.path().join("a.md")).unwrap(),
        body,
        "文件必须真的回到了原处"
    );
    assert!(
        block_on(trash_list(root)).unwrap().is_empty(),
        "条目必须已经离开回收站"
    );
}

/* ───────────────────── 外部删除 ───────────────────── */

/// 条目被外部删掉 → 恢复报 `IO / NOT_FOUND`，不是内部错误。
///
/// 错误码决定前端给什么建议：`NOT_FOUND` 说"刷新列表"，而 `INTERNAL` 会让
/// 用户去删索引。前者是这次真正该做的事。
#[test]
fn restoring_a_vanished_entry_reports_not_found() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    fs::write(vault.path().join("a.md"), "内容").unwrap();

    scan(&root);
    let trashed = block_on(note_delete(root.clone(), "a.md".to_owned())).expect("删除失败");
    let name = trashed.rsplit('/').next().unwrap().to_owned();
    fs::remove_file(vault.path().join(&trashed)).unwrap();

    assert!(block_on(trash_list(root.clone())).unwrap().is_empty());

    let err = block_on(trash_restore(root, name)).expect_err("文件没了，恢复必须失败");
    match err {
        KiError::Io { kind, .. } => assert_eq!(kind, IoKind::NotFound, "实际 {kind:?}"),
        other => panic!("应当是 IO/NotFound，实际 {other:?}"),
    }
}
