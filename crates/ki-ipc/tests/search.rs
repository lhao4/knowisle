//! 进程级缓存的行为契约：并发只建一份索引，换库重建，重新扫描后重建。
//!
//! 这些用例碰的是 `ki-ipc::search` 的全局状态，所以**必须放在同一个 test 函数里**
//! 顺序执行——分成多个 `#[test]` 会被 cargo 并行跑，互相把对方的缓存踩掉。

use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

use ki_ipc::search;

fn vault(files: usize, body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    for i in 0..files {
        fs::write(dir.path().join(format!("n{i:03}.md")), body).expect("写文件失败");
    }
    dir
}

#[test]
fn cache_builds_once_per_root_and_rebuilds_after_invalidate() {
    let dir = vault(30, "并发探针\n");
    let expected = 30;

    // 五个线程同时开搜：模拟「连打 5 个字符就是 5 次搜索」。
    let barrier = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();
    for _ in 0..5 {
        let root = dir.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            // 五个线程在同一时刻冲出栅栏，谁先拿到锁谁建索引。
            barrier.wait();
            search::query(&root, "并发探针", None).expect("搜索失败")
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("线程 panic"))
        .collect();

    assert!(
        results.iter().all(|result| result.builds == 1),
        "并发时必须只建一份索引，实际：{:?}",
        results.iter().map(|r| r.builds).collect::<Vec<_>>()
    );
    assert!(results.iter().all(|result| result.total == expected));
    // 建索引耗时只可能由"真正建了"的那一个请求报告。
    assert!(
        results.iter().filter(|result| result.index_ms > 0).count() <= 1,
        "最多一个请求报告建索引耗时"
    );

    // 重新扫描 = 用户明确要求"再看一眼磁盘"：缓存必须作废。
    search::invalidate(dir.path());
    let rebuilt = search::query(dir.path(), "并发探针", None).expect("重建失败");
    assert_eq!(rebuilt.builds, 2, "invalidate 之后必须重建，计数继续累加");

    // 换库：计数从 1 重新开始，旧索引被丢掉。
    let other = vault(3, "另一个库\n");
    let switched = search::query(other.path(), "另一个库", None).expect("换库失败");
    assert_eq!(switched.builds, 1);
    assert_eq!(switched.indexed_files, 3);
}
