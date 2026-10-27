//! FTS 引擎（`service::search`）自己的行为：刷新当前篇、诊断计数、候选池饱和。
//!
//! # 为什么这些用例要串行
//!
//! `ki_ipc::search` 的缓存是**进程全局**的。cargo 会把同一个测试二进制里的
//! `#[test]` 并行跑，于是两个用例会互相把对方的 Vault 换掉——`builds` 与
//! `index_ms` 这类诊断字段立刻变成随机值。
//!
//! 每个用例用自己的临时目录，所以**切换 root 会把 `builds` 与 `index_ms` 归零**，
//! 只要串行执行，每个用例就都从干净的计数开始。因此这里用一个进程内的
//! `Mutex` 把它们排成队，而不是把所有断言塞进一个巨型 `#[test]`——
//! 后者失败时无法定位到具体哪一节。
//!
//! 内容层面的断言（命中什么、命中几行）不依赖缓存计数，本来就不怕并行；
//! 串行只是为了让断言确定，不是为了正确性。

use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex, MutexGuard, PoisonError};
use std::thread;

use ki_ipc::search::{service, sync_after_scan};
use ki_vault::{scan_vault, ScanOptions};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    // 某个用例 panic 之后其余用例仍要能跑：拿回内部值即可。
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn vault() -> tempfile::TempDir {
    tempfile::tempdir().expect("创建临时目录")
}

fn index(root: &Path) {
    let scan = scan_vault(root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(root, &scan).expect("同步");
}

/// 刷新当前编辑篇：磁盘改了但没重新扫描，也要立刻能搜到。
///
/// 这是「刚打的字搜不到」这个体验问题的唯一解：每次搜索只重读**一篇**。
#[test]
fn refresh_path_makes_unsaved_edits_searchable() {
    let _guard = serial();
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha 原内容").expect("写");
    index(dir.path());

    // 模拟编辑器落盘但还没重新扫描
    fs::write(dir.path().join("a.md"), "alpha 新内容 岛屿检索").expect("改写");

    let without = service::search(dir.path(), "岛屿检索", None).expect("不刷新");
    assert!(
        without.hits.is_empty(),
        "没刷新之前索引里还是旧 token，不该搜到新词"
    );

    let with = service::search(dir.path(), "岛屿检索", Some("a.md")).expect("刷新后");
    assert_eq!(with.hits.len(), 1, "刷新当前篇之后必须立刻搜到");
    assert_eq!(with.hits[0].path, "a.md");

    // 刷新之后索引真的变了：再搜一次（不带 refresh）也能搜到
    let again = service::search(dir.path(), "岛屿检索", None).expect("再搜");
    assert_eq!(again.hits.len(), 1, "刷新已经写进索引库了");
}

/// 刷新不认识的路径时什么都不做——新文件要等下一次扫描才可见。
///
/// 这是 B-09 的既有边界（没有文件监听），不能在这里偷偷补一个。
#[test]
fn refresh_path_ignores_files_outside_the_index() {
    let _guard = serial();
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha").expect("写");
    index(dir.path());

    fs::write(dir.path().join("b.md"), "bravo 新文件").expect("写新文件");
    let result = service::search(dir.path(), "bravo", Some("b.md")).expect("搜索");
    assert!(result.hits.is_empty(), "索引里没有的文件不该被偷偷加进来");
}

/// 从没扫描过的库：搜索**自己先扫一遍**，而不是静默返回空。
///
/// 旧引擎的 `query` 是自足的（没索引就当场建）。新引擎若不自足，
/// 「直接调 `vault_search` 而没先 `vault_scan`」会返回空结果——不报错，
/// 只是找不到东西，这是最糟的失败模式。
#[test]
fn search_without_a_prior_scan_syncs_first() {
    let _guard = serial();
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha").expect("写");
    fs::write(dir.path().join("b.md"), "bravo").expect("写");

    let result = service::search(dir.path(), "alpha", None).expect("搜索");
    assert_eq!(result.hits.len(), 1, "没扫描过也要能搜到");
    assert_eq!(result.indexed_files, 2);
    assert_eq!(result.builds, 1, "第一次搜索要把索引库打开");

    // 第二次搜索不该再扫一遍：`synced_at` 已经落下。
    let again = service::search(dir.path(), "bravo", None).expect("再搜");
    assert_eq!(again.hits.len(), 1);
    assert_eq!(again.builds, 1, "不该重新打开索引库");
}

/// `index_ms` 只被报告一次：它是「本次搜索之前最近一次索引维护耗时」。
///
/// UI 的判据是「冷启动第一次搜索显示毫秒，第二次为 0」。不取走清零的话，
/// 每次搜索都会显示一个假耗时。
#[test]
fn index_ms_is_reported_once_after_maintenance() {
    let _guard = serial();
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha").expect("写");
    index(dir.path());

    let first = service::search(dir.path(), "alpha", None).expect("第一次");
    assert!(first.index_ms > 0, "扫描之后第一次搜索必须报告索引耗时");

    let second = service::search(dir.path(), "alpha", None).expect("第二次");
    assert_eq!(second.index_ms, 0, "耗时只该被报告一次");
    assert_eq!(second.builds, first.builds, "第二次不该重新打开索引库");
}

/// 并发搜索只能打开一次索引库。
///
/// 与旧引擎的 `cache_builds_once_per_root_...` 是同一条契约：连打 5 个字符
/// 就是 5 次搜索，锁必须让后来者排队，而不是各开一个连接各做一次体检。
#[test]
fn concurrent_searches_open_the_index_once() {
    let _guard = serial();
    let dir = vault();
    for i in 0..10 {
        fs::write(dir.path().join(format!("n{i}.md")), "alpha").expect("写");
    }

    let barrier = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();
    for _ in 0..5 {
        let root = dir.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            service::search(&root, "alpha", None).expect("搜索")
        }));
    }
    let builds: Vec<u32> = handles
        .into_iter()
        .map(|handle| handle.join().expect("线程 panic").builds)
        .collect();

    assert!(
        builds.iter().all(|&count| count == 1),
        "并发时只该打开一次索引库，实际 {builds:?}"
    );
}

/// 候选都在预算之内时，`total` 是**准确值**——不是下界。
///
/// 205 篇各一条命中：`hits` 被截到 200 条，但那是「只返回前 200 条」，
/// 与「数不清」是两件事。旧实现把两者塞进同一个 `truncated`，于是
/// 「共 205 条」被 UI 说成「至少 205 条」——准确值被降级成了下界。
#[test]
fn pool_within_budget_reports_an_exact_total() {
    let _guard = serial();
    let dir = vault();
    for i in 0..205 {
        fs::write(dir.path().join(format!("n{i:03}.md")), "alpha").expect("写");
    }
    index(dir.path());

    let result = service::search(dir.path(), "alpha", None).expect("搜索");
    assert_eq!(result.hits.len(), 200);
    assert_eq!(result.total, 205, "候选全在手里，total 必须是准确值");
    assert!(!result.truncated, "total 准确时不许标成下界");
}

/// 候选超过 `POOL_EXACT_MAX` 时，池子按 `POOL_SCAN_MAX` 截断，`total`
/// 降级为下界，结果**必须**标成 `truncated`——否则 UI 会把「至少 200 条」
/// 说成「共 200 条」。
///
/// 夹具是 1201 篇（刚过预算一条），所以这条用例慢（建索引十秒量级）。
/// 边界本身（1200 / 1201）由 `service.rs` 里的 `pool_plan` 单测钉住，
/// 这里只需要证明「超预算 → truncated」这条线真的接上了。
#[test]
fn pool_over_budget_marks_the_result_as_truncated() {
    let _guard = serial();
    let dir = vault();
    for i in 0..1201 {
        fs::write(dir.path().join(format!("n{i:04}.md")), "alpha").expect("写");
    }
    index(dir.path());

    let result = service::search(dir.path(), "alpha", None).expect("搜索");
    assert_eq!(result.hits.len(), 200);
    assert!(
        result.truncated,
        "候选超过预算时 truncated 必须为真（total 已是下界）"
    );
    assert_eq!(result.total, 200, "下界 = 池内命中行数（每篇一条）");
    assert!(result.total < 1201, "下界当然小于全库命中数");
}
