//! 命令层测试。
//!
//! 这里**不重复** `ki-vault` 已经覆盖的边界（路径穿越、软链、原子性）。
//! 本文件只回答一个问题：**命令接线对不对**——
//! 参数顺序、返回值、错误能否原样穿出 `spawn_blocking`。
//!
//! 这类错误在 `src-tauri` 时代是测不到的：那边要链接 webview，`cargo test` 跑不起来。
//! 而"`note_save` 的第二个参数接错了字段"恰恰是最容易犯、最难发现的一类错误。

use ki_ipc::{app_ping, note_read, note_save, vault_scan};
use tempfile::TempDir;

/// `#[tauri::command]` 的 async 版本返回 Future，测试里自己驱动它。
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tauri::async_runtime::block_on(future)
}

#[test]
fn note_save_then_note_read_round_trips() {
    let vault = TempDir::new().unwrap();
    let root = vault.path().to_string_lossy().into_owned();
    let content = "# 从命令层写进去的\n\n中文与 emoji 🚀\n";

    let saved = block_on(note_save(
        root.clone(),
        "a.md".to_owned(),
        content.to_owned(),
    ))
    .expect("note_save 应当成功");

    assert_eq!(saved.path, "a.md");
    assert_eq!(saved.size, content.len() as u64);

    let read = block_on(note_read(root, "a.md".to_owned())).expect("note_read 应当成功");
    assert_eq!(read.content, content);
    assert_eq!(read.size, saved.size);
}

/// 写完之后立刻扫描，树里必须能看到这篇笔记。
///
/// 这条锁的是"写了但看不见"——扫描器的扩展名过滤与写入的落点必须一致。
#[test]
fn written_note_shows_up_in_the_scan_tree() {
    let vault = TempDir::new().unwrap();
    let root = vault.path().to_string_lossy().into_owned();
    std::fs::create_dir(vault.path().join("notes")).unwrap();

    block_on(note_save(
        root.clone(),
        "notes/fresh.md".to_owned(),
        "hi".to_owned(),
    ))
    .unwrap();

    let scan = block_on(vault_scan(root)).unwrap();
    assert_eq!(scan.total_files, 1);

    fn find<'a>(node: &'a ki_core::FileNode, path: &str) -> Option<&'a ki_core::FileNode> {
        if node.path == path {
            return Some(node);
        }
        node.children.iter().flatten().find_map(|c| find(c, path))
    }
    let node = find(&scan.tree, "notes/fresh.md").expect("树里应当有 notes/fresh.md");
    assert_eq!(node.size, 2);
}

/// 错误必须原样穿过 `spawn_blocking` 回到调用方，不能被包成 `Internal`。
#[test]
fn domain_errors_survive_the_thread_hop() {
    let vault = TempDir::new().unwrap();
    let root = vault.path().to_string_lossy().into_owned();

    let err = block_on(note_save(root, "../escape.md".to_owned(), "x".to_owned()))
        .expect_err("路径逃逸应当失败");

    let debug = format!("{err:?}");
    assert!(debug.contains("PathEscape"), "实际 {debug}");
    assert!(
        !debug.contains("Internal"),
        "错误不该被 spawn_blocking 吞成 Internal：{debug}"
    );
}

/// `app_ping` 的版本号必须与 crate 版本同源。
#[test]
fn app_ping_reports_the_crate_version() {
    let info = app_ping();
    assert_eq!(info.name, "knowisle");
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    assert!(info.pid > 0);
}
