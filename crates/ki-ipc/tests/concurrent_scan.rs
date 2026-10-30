//! 两次 `vault_scan` **并发**时，索引同步不许互相踩踏。
//!
//! # 这一条守的是什么
//!
//! React StrictMode 会把「恢复上次的库」这个 effect 挂载两次，于是启动时有
//! **两次** `vault_scan`：两次 `invalidate`、两次扫描、两次 `spawn_after_scan`。
//! 代际（ADR-009）保证「旧任务不发布状态」，但**不保证它不写库**——它在下一个
//! 批次边界才退出，而那可能是 250 ms 之后。
//!
//! 实测（本用例的前身，8 轮里的第 3 轮）：两个线程同时 `IndexDb::open` 同一个
//! WAL 库，其中一个的写拿到 `SQLITE_READONLY`
//! （`attempt to write a readonly database`），整次同步作废、索引停在上一版。
//! 用户看到的现象是「删掉的笔记还留在跳过清单里」，而重跑一次又好了——
//! 最难查的那一类。
//!
//! 修法见 `search::cache::sync_gate`：同一时刻只允许一个同步在跑，门覆盖
//! 整个同步（含 `IndexDb::open`），不只是写库那一段。
//!
//! # 为什么断言「删掉的那几篇搜不到」
//!
//! 只断言 `indexState != Failed` 的话，一个「不报错但什么都没写」的实现
//! 也能绿。用户能感知的判据只有一个：**删掉的笔记不该还留在索引里**。

use std::fs;
use std::path::Path;

use ki_ipc::search::{encoding_stats, invalidate, service, spawn_after_scan};
use ki_vault::{scan_vault, ScanOptions};

fn write(root: &Path, rel: &str, body: &str) {
    let abs = root.join(rel);
    fs::write(&abs, body).expect("写盘");
}

/// 应用的 `vault_scan` 命令逐字照抄：作废旧缓存 → 扫描 → 后台建索引。
fn vault_scan(root: &Path) {
    invalidate(root);
    let scan = scan_vault(root, &ScanOptions::default()).expect("扫描");
    spawn_after_scan(root, &scan);
}

/// 等到后台任务给出终态（`Ready` 或 `Failed`）。
fn wait_terminal(root: &Path) -> ki_core::IndexState {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let state = encoding_stats(root).expect("统计").index_state;
        if !matches!(state, ki_core::IndexState::Building { .. }) {
            return state;
        }
        assert!(std::time::Instant::now() < deadline, "后台索引 20 秒没给终态");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn concurrent_scans_do_not_leave_the_index_stale() {
    let dir = tempfile::tempdir().expect("临时目录");
    let root = dir.path();

    let total = 24usize;
    for i in 0..total {
        write(root, &format!("n{i:02}.md"), &format!("# 第 {i} 篇\n"));
    }
    vault_scan(root);
    assert!(matches!(wait_terminal(root), ki_core::IndexState::Ready));

    // 每一轮：删一篇 → 两次扫描并发（StrictMode 的双挂载）。
    let rounds = 6usize;
    for i in 0..rounds {
        fs::remove_file(root.join(format!("n{i:02}.md"))).expect("删掉");

        let a = root.to_path_buf();
        let b = root.to_path_buf();
        let ta = std::thread::spawn(move || vault_scan(&a));
        let tb = std::thread::spawn(move || vault_scan(&b));
        ta.join().expect("第一个扫描线程");
        tb.join().expect("第二个扫描线程");

        let state = wait_terminal(root);
        assert!(
            !matches!(state, ki_core::IndexState::Failed { .. }),
            "第 {i} 轮索引失败了：{state:?}"
        );
    }

    // 用户能感知的那一条：删掉的六篇不该还搜得到。
    let result = service::search(root, "第", None).expect("搜索");
    assert_eq!(
        result.total,
        total - rounds,
        "索引没跟上删除：应剩 {} 篇，实际 {} 条",
        total - rounds,
        result.total
    );
}
