//! 后台索引的三条硬契约：**切库即失效**、**搜索不排队**、**半成品能自愈**。
//!
//! # 为什么这三条必须是自动化用例
//!
//! 它们都是「不做也不会报错」的：
//!
//! - 切库不失效 → 旧任务继续往旧库里写。界面上一切正常，只是切回来时索引
//!   里混着两次扫描之间的行。
//! - 搜索排队 → 建库那 100 秒里搜索转圈。用户以为"搜不出来"，其实是在等锁。
//! - 半成品不自愈 → 崩过一次之后，某些笔记**永远**搜不到，直到整库重建。
//!
//! 手测抓不到：前两条要卡在"建到一半"的窗口里，第三条要真的崩一次。
//! 所以这里用 `service::hooks::AFTER_BATCH` 把「第 N 批之后」变成**确定的**
//! 时点——靠夹具规模去卡时间窗，在 Linux 上 1000 篇只要几十毫秒（假绿），
//! 在慢机器上又可能假红。
//!
//! # 钩子是全局静态
//!
//! 同一个测试二进制里的用例默认并行跑，而钩子、代际、缓存都是进程级的。
//! 每个用例都先拿 [`serial`] 那把锁——**不要**为了让它们跑得快而拿掉，
//! 那会得到"三个用例互相把对方的建索引任务作废"的随机失败。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ki_core::IndexState;
use ki_ipc::search::{index_state_for_test, service, spawn_after_scan, sync_after_scan};
use ki_store::{Counts, IndexDb};
use ki_vault::{scan_vault, ScanOptions};

/// 用例串行锁。见文件头。
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    // 上一个用例 panic 过也继续：钩子由每个用例自己重装。
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 每篇约 220 字节，与 `index_bench.rs` 同口径（中文 3 字节/字）。
fn note_body(index: usize) -> String {
    format!(
        "# 笔记 {index:05}\n\n\
         本地优先的知识库把正文留在磁盘上，索引只是派生数据。\
         这一篇用来测量建库时每篇的固定开销：路径解析、读盘、分词、写库。\
         编号 {index:05} 让它唯一，也让它可被定位器命中。\n"
    )
}

/// 幂等的夹具：同名目录整个重建，跑多少次起点都一样（B-32）。
///
/// 放系统临时目录而不是仓库的 `.tmp/`：那里删一个文件 390ms（本机实测），
/// 几百个文件就要几分钟。
fn fixture(name: &str, files: usize) -> PathBuf {
    let root = std::env::temp_dir().join(name);
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("清掉上一轮的夹具");
    }
    std::fs::create_dir_all(&root).unwrap();
    for index in 0..files {
        std::fs::write(root.join(format!("n{index:05}.md")), note_body(index)).unwrap();
    }
    root
}

/// 绕开应用连接直接读库：切库之后 `search` 会自愈重建，拿它当量具会改变被测状态。
fn counts(root: &Path) -> Counts {
    let (db, _) = IndexDb::open(root).expect("打开索引库");
    db.counts().expect("读计数")
}

fn synced_at(root: &Path) -> i64 {
    let (db, _) = IndexDb::open(root).expect("打开索引库");
    db.synced_at().expect("读 synced_at")
}

/// 一个只能被放行一次的闸门。
///
/// 用它把后台线程**停在两批之间**：那时它既没持 SQLite 写锁，也没持 `CACHE`，
/// 于是"搜索会不会被挡住"这件事才测的是锁，而不是别的东西。
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        while !*open {
            open = self.cv.wait(open).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_all();
    }
}

fn install(hook: Option<Box<dyn FnMut(usize) -> bool + Send>>) {
    *service::hooks::AFTER_BATCH
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = hook;
}

/// 「第 1 批提交之后通知我，然后卡住等我放行」。
fn hold_after_first_batch() -> (Receiver<()>, Arc<Gate>) {
    let (tx, rx) = channel();
    let gate = Arc::new(Gate::default());
    let thread_gate = Arc::clone(&gate);
    install(Some(Box::new(move |batches| {
        if batches == 1 {
            let _ = tx.send(());
            thread_gate.wait();
        }
        true
    })));
    (rx, gate)
}

/// 切库：库 A 建到一半时切到 B，A 的索引库**不许再增长**。
///
/// 判据是行数，不是"线程退出了没有"——用户能观察到的只有前者。
#[test]
fn switching_vault_stops_the_old_build() {
    let _serial = serial();
    let a = fixture("ki-bg-switch-a", 600);
    let b = fixture("ki-bg-switch-b", 5);

    let (notified, gate) = hold_after_first_batch();
    let scan_a = scan_vault(&a, &ScanOptions::default()).unwrap();
    spawn_after_scan(&a, &scan_a);

    notified
        .recv_timeout(Duration::from_secs(30))
        .expect("A 的第一批应当在 30 秒内提交");
    let half = counts(&a);
    assert!(half.ok > 0, "A 至少写进了第一批");
    assert!(half.ok < 600, "A 应当停在中途，实际已写 {} 篇", half.ok);

    // 用户切库：`vault_scan(B)` 做的就是 invalidate + 开新一代。
    let scan_b = scan_vault(&b, &ScanOptions::default()).unwrap();
    spawn_after_scan(&b, &scan_b);

    gate.release();

    // 「等 5 秒再看」：给旧任务足够的时间把剩下的批次写完——如果它没失效的话。
    std::thread::sleep(Duration::from_secs(5));
    let after = counts(&a);
    std::thread::sleep(Duration::from_secs(1));
    let later = counts(&a);

    assert_eq!(
        after, later,
        "切库之后 A 的索引库不许再增长（{} → {}）",
        after.ok, later.ok
    );
    assert_eq!(after, half, "切库之后 A 一行都不该多写");
    assert!(
        !matches!(index_state_for_test(&a), IndexState::Building { .. }),
        "旧任务不许再宣称 A 正在建（状态是 {:?}）",
        index_state_for_test(&a)
    );

    // B 自己必须建完——否则这条用例只能证明"什么都没发生"。
    let deadline = Instant::now() + Duration::from_secs(30);
    while index_state_for_test(&b) != IndexState::Ready {
        assert!(Instant::now() < deadline, "B 的索引没能在 30 秒内就绪");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(counts(&b).ok, 5);
    install(None);
}

/// 搜索不排队：后台卡在两批之间时，搜索必须在毫秒级返回。
///
/// 变异点：后台线程改成复用 `CACHE` 里的连接（即持锁）→ 这里会等满闸门的
/// 3 秒，断言变红。
#[test]
fn search_does_not_wait_for_the_background_build() {
    let _serial = serial();
    let root = fixture("ki-bg-search", 600);

    let (notified, gate) = hold_after_first_batch();
    let scan = scan_vault(&root, &ScanOptions::default()).unwrap();
    spawn_after_scan(&root, &scan);
    notified
        .recv_timeout(Duration::from_secs(30))
        .expect("第一批应当在 30 秒内提交");

    let started = Instant::now();

    // 看门狗：搜索若被锁死，3 秒后自己放行。
    //
    // 没有它的话，"后台持锁"这个变异不会让用例变红，而是让它**挂死**——
    // 因为闸门只有搜索返回之后才放行（变异验证实测：150 秒超时被杀，exit 143）。
    // 挂死的用例在 CI 上只表现为作业超时，看不出是谁的问题。
    let watchdog = Arc::clone(&gate);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        watchdog.release();
    });

    let result = service::search(&root, "笔记 001", None).expect("索引在建时搜索也要能出结果");
    let elapsed = started.elapsed();

    // 先放行再断言：断言失败时不要把后台线程永远挂在这里。
    gate.release();

    assert!(
        elapsed < Duration::from_secs(1),
        "搜索被后台建索引挡住了：{elapsed:?}"
    );
    assert!(
        matches!(result.index_state, IndexState::Building { .. }),
        "索引没建完时必须报 Building，实际 {:?}",
        result.index_state
    );
    if let IndexState::Building { done, total } = result.index_state {
        assert!(
            done < total,
            "Building 必须如实报告进度，实际 {done}/{total}"
        );
        assert!(done > 0, "第一批已经提交了，done 不该是 0");
    }
    assert!(
        result.indexed_files < 600,
        "这时只该看到部分索引，实际 {} 篇",
        result.indexed_files
    );
    install(None);
}

/// 半成品索引库：只写前一批 → 再跑完整同步 → 与一次建成逐字段相等。
///
/// 这条钉的是「崩溃后自愈」这条**理论**。`sync` 每 256 篇提交一次、
/// `synced_at` 最后写，所以崩溃后的状态是"前 N 批已落库、时间戳还是旧的"。
/// 下次扫描按 `size + mtime` 补差——但在此之前没有任何用例证明它真的补得上。
#[test]
fn a_half_written_index_is_healed_by_the_next_sync() {
    let _serial = serial();
    let root = fixture("ki-bg-heal", 300);

    // 第 1 批之后停下：等价于"写到一半进程没了"。
    install(Some(Box::new(|batches| batches < 1)));
    let scan = scan_vault(&root, &ScanOptions::default()).unwrap();
    let report = sync_after_scan(&root, &scan).expect("同步本身不该报错");
    assert_eq!(report.stats.inserted, 256, "只该写进第一批");
    assert_eq!(counts(&root).ok, 256);
    assert_eq!(
        synced_at(&root),
        0,
        "中途停下**不许**写 synced_at——那会让下一次搜索以为索引是全的"
    );

    install(None);
    sync_after_scan(&root, &scan).expect("补齐");

    let healed = counts(&root);
    assert_eq!(healed.ok, 300, "剩下的 44 篇必须补上");

    // 对照：同一份夹具从零建一次。
    let control = fixture("ki-bg-heal-control", 300);
    let control_scan = scan_vault(&control, &ScanOptions::default()).unwrap();
    sync_after_scan(&control, &control_scan).expect("对照建库");

    assert_eq!(
        healed,
        counts(&control),
        "补齐之后的索引必须与一次建成逐字段相等"
    );
}
