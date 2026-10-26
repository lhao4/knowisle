//! 索引建库的基准：10k 篇、release 构建、median of 5。
//!
//! # 跑法
//!
//! ```sh
//! cargo test -p ki-ipc --release --test index_bench -- --ignored --nocapture
//! ```
//!
//! 只有 release 的数字有意义。debug 下 `serde_json`、`read_dir`、FTS5 写入
//! 慢 5–20 倍，拿它估出来的比例没有解释力——ADR-007 第一版的「10k 篇约 5 秒」
//! 就是这么拍出来的，release 实测是 16–108 秒。
//!
//! # 为什么这次基准住在版本库里
//!
//! M2–M5 的测量脚本全是一次性的（`.tmp/bench-*.mjs`，不入库），于是
//! 「上一刀改完到底快了多少」每次都只能靠回忆。B-27 这一刀动的是所有读路径，
//! 它的数字必须能被下一次改动复现，否则下一个优化者面对的是「据说以前很慢」。
//!
//! # 夹具
//!
//! 10,000 篇 / 50 个父目录（每目录 200 篇）/ 每篇约 220 字节正文。
//!
//! **50 个父目录是刻意的**：本刀把「父目录链的重解析点检查」从逐篇改成
//! 按**去重后的父目录集合**做一次，父目录数决定这条优化的上限。全塞在库根
//! （1 个父目录）会把这个收益放大到不真实；一篇一个目录（10k 个父目录）
//! 又会把收益抹成 0。50 是「一个整理得还行的库」的常见形状。
//!
//! 夹具在 `%TEMP%/knowisle-index-bench`。**不能放仓库的 `.tmp/`**：那里
//! 删一个文件 390ms（本机实测），清 10k 个要一个多小时；`%TEMP%` 里 1.8ms。
//!
//! # 冷路径测不准，所以分两段
//!
//! Windows 没有公开、可靠、且只影响本进程的办法把文件从缓存里踢出去
//! （`SetSystemFileCacheSize` 是整机级的，且需要特权）。所以：
//!
//! - **first**：刚写完夹具就测的那一次。文件还在写回缓存里，比真正的冷启动
//!   乐观得多——M4 的「100.9s」就是这个口径，本文件的 first 与它可比。
//! - **warm**：紧接着的 5 次，取 median。这是本刀的主判据。
//!
//! 两段都打印。**别只挑好看的那个**：本刀只优化「每篇的路径闸开销」，
//! 杀软扫描那部分（first 与 warm 的差额）一分钱都不该变。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ki_core::{IndexState, ScanResult};
use ki_ipc::search::{index_state_for_test, invalidate, sync_after_scan};
use ki_vault::{flatten_files, scan_vault, ScanOptions};

/// 夹具篇数。与 ADR-007 / BACKLOG B-27 的实测口径一致。
const FILES: usize = 10_000;
/// 父目录数。见文件头的「50 个父目录是刻意的」。
const DIRS: usize = 50;

/// 两个 ignore 用例共用同一份夹具，而 `cargo test -- --ignored` 会**并行**跑它们。
/// 没有这把锁的话，`ten_thousand_notes_index_build` 的 `build_fixture` 会把
/// 正在被 `per_file_cost_breakdown` 读的目录删掉——实测报 `NotFound`，
/// 看起来像实现坏了。
static FIXTURE: Mutex<()> = Mutex::new(());

fn fixture_lock() -> MutexGuard<'static, ()> {
    FIXTURE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-index-bench")
}

/// 一篇约 220 字节的正文（中文 3 字节/字）。
///
/// 长度不是随便定的：M4 实测 10,003 篇正文合计 2.0 MiB，即约 210 字节/篇。
/// 正文再小，读盘成本会被 `open` 的固定开销淹没；再大，测的就变成磁盘带宽
/// 而不是本刀要改的路径闸。
fn note_body(index: usize) -> String {
    format!(
        "# 笔记 {index:05}\n\n\
         本地优先的知识库把正文留在磁盘上，索引只是派生数据。\
         这一篇用来测量建库时每篇的固定开销：路径解析、读盘、分词、写库。\
         编号 {index:05} 让它唯一，也让它可被定位器命中。\n"
    )
}

fn build_fixture(root: &Path) {
    if root.exists() {
        fs::remove_dir_all(root).expect("清掉上一轮的夹具");
    }
    for dir in 0..DIRS {
        fs::create_dir_all(root.join(format!("d{dir:02}"))).unwrap();
    }
    for index in 0..FILES {
        let dir = format!("d{:02}", index % DIRS);
        let path = root.join(dir).join(format!("n{index:05}.md"));
        fs::write(path, note_body(index)).unwrap();
    }
}

/// 把索引库整个删掉，让下一次 `sync_after_scan` 走「新建 + 全量读」。
///
/// `invalidate` 先丢进程内的连接：只删文件不丢连接的话，下一次同步会往一个
/// 已经被 unlink 的 inode 里写，测出来的数字漂亮但毫无意义。
fn reset_index(root: &Path) {
    invalidate(root);
    let meta = root.join(ki_core::METADATA_DIR);
    if meta.exists() {
        fs::remove_dir_all(&meta).expect("删掉上一轮的索引库");
    }
}

/// `#[tauri::command]` 的 async 版本返回 Future，测试里自己驱动它。
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tauri::async_runtime::block_on(future)
}

/// 一次「建库」：扫描（计时外）+ 同步（计时内）。
///
/// 扫描不计时是因为本刀没动它；但它的耗时照样打印，因为「树的可见时间」
/// 是 `scan + sync`——验收要求的是**顺序不变**，不是这两项之和不变。
fn run_once(root: &Path) -> Sample {
    let scan = scan_vault(root, &ScanOptions::default()).unwrap();
    let started = Instant::now();
    let report = sync_after_scan(root, &scan).unwrap();
    Sample {
        scan_ms: scan.elapsed_ms,
        sync_ms: started.elapsed().as_millis() as u64,
        scan: summarize(&scan),
        stats: format!("{:?}", report.stats),
    }
}

/// 一次运行的完整可对比快照。
///
/// 除了耗时，还带上「语义指纹」：树 / skipped / 索引行数。验收要求
/// 「同一夹具下逐字段与改动前相等」——把它们打在同一行里，两次运行的
/// diff 就是验收证据，不需要再单独写一个对拍脚本。
struct Sample {
    scan_ms: u64,
    sync_ms: u64,
    scan: String,
    stats: String,
}

impl Sample {
    fn total_ms(&self) -> u64 {
        self.scan_ms + self.sync_ms
    }
}

fn summarize(scan: &ScanResult) -> String {
    format!(
        "files={} dirs={} skipped={} truncated={} tree={:016x}",
        scan.total_files,
        scan.total_dirs,
        scan.skipped,
        scan.truncated,
        fingerprint(scan)
    )
}

/// 树内容的 FNV-1a 指纹：路径 + 字节数，按路径排序后累加。
///
/// 用指纹而不是逐行 dump：10k 行输出没人会看第二眼，而指纹变了就说明
/// 「树变了」这件事本身需要解释。真要看差异时把 `FILES` 调小到 20 再跑。
fn fingerprint(scan: &ScanResult) -> u64 {
    let mut files = flatten_files(&scan.tree);
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in &files {
        for byte in file.path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= file.size;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn median(samples: &[Duration]) -> f64 {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2].as_secs_f64() * 1000.0
}

#[test]
#[ignore = "10k 夹具 + 6 次全量建库要几分钟，显式跑：cargo test -p ki-ipc --release --test index_bench -- --ignored --nocapture"]
fn ten_thousand_notes_index_build() {
    let _fixture = fixture_lock();
    let root = fixture_root();
    build_fixture(&root);

    // ── first：刚写完就测。与 M4 的「100.9s（刚写完首次打开）」同口径。 ──
    reset_index(&root);
    let first = run_once(&root);
    println!(
        "first（刚写完夹具，缓存未必冷）: scan {}ms + sync {}ms = {}ms | {} | {}",
        first.scan_ms,
        first.sync_ms,
        first.total_ms(),
        first.scan,
        first.stats
    );

    // ── warm：5 次，median。这是主判据。 ──
    let mut syncs: Vec<Duration> = Vec::with_capacity(5);
    for round in 0..5 {
        reset_index(&root);
        let sample = run_once(&root);
        println!(
            "warm[{round}]: scan {}ms + sync {}ms = {}ms | {} | {}",
            sample.scan_ms,
            sample.sync_ms,
            sample.total_ms(),
            sample.scan,
            sample.stats
        );
        syncs.push(Duration::from_millis(sample.sync_ms));
    }

    println!(
        "warm median: sync {:.0}ms（5 次；夹具 {} 篇 / {} 个父目录 / {}）",
        median(&syncs),
        FILES,
        DIRS,
        root.display()
    );

    // 语义零变化：同一份夹具下两次运行的指纹必须相同。
    // 这条断言在本文件里只是自检（同一份代码跑两次），真正的对比是
    // 改动前后各跑一次、diff 上面那几行 `files=… tree=…`。
    let again = {
        reset_index(&root);
        run_once(&root)
    };
    assert_eq!(first.scan, again.scan, "同一夹具两次扫描的指纹必须相同");
}

/// 分项：每篇的固定开销到底花在哪。
///
/// 这个用例是「5× 目标没达到」时唯一能指出下一步该改什么的东西。B-27 的教训
/// 是**别猜**：M4 把「10k 篇约 5 秒」写进 ADR 时靠的就是猜，实测 16–108 秒。
/// 本刀第一版只省掉两次 `canonicalize`，热路径从 18.4s 降到 11.4s（1.6×），
/// 说明剩下的钱不在那里——下面这几行就是用来找它在哪的。
///
/// ```sh
/// cargo test -p ki-ipc --release --test index_bench -- breakdown --ignored --nocapture
/// ```
#[test]
#[ignore = "分项探针，显式跑：cargo test -p ki-ipc --release --test index_bench -- breakdown --ignored --nocapture"]
fn per_file_cost_breakdown() {
    let _fixture = fixture_lock();
    let root = fixture_root();
    if !root.exists() {
        build_fixture(&root);
    }

    let vault = ki_vault::ResolvedVault::open(&root).unwrap();
    let mut paths: Vec<String> = (0..FILES)
        .map(|index| format!("d{:02}/n{index:05}.md", index % DIRS))
        .collect();
    paths.sort();

    // 预热一遍：第一轮包含文件系统元数据缓存与 Defenders 的首次判定。
    let _ = std::fs::read(root.join(&paths[0])).unwrap();
    let _ = vault.resolve(&paths[0]).unwrap();

    let time = |label: &str, mut f: Box<dyn FnMut(&str) + '_>| {
        let started = Instant::now();
        for path in &paths {
            f(path);
        }
        let per = started.elapsed().as_secs_f64() * 1e6 / paths.len() as f64;
        println!(
            "{label:<34} {:>8.0}ms total  {:>7.1}µs/篇",
            started.elapsed().as_secs_f64() * 1000.0,
            per
        );
    };

    time(
        "resolve（父目录缓存命中）",
        Box::new(|path| {
            let _ = vault.resolve(path).unwrap();
        }),
    );
    time(
        "fs::metadata（path stat）",
        Box::new(|path| {
            let _ = std::fs::metadata(root.join(path)).unwrap();
        }),
    );
    time(
        "File::open（只开不读）",
        Box::new(|path| {
            let _ = std::fs::File::open(root.join(path)).unwrap();
        }),
    );
    time(
        "File::open + file.metadata()",
        Box::new(|path| {
            let file = std::fs::File::open(root.join(path)).unwrap();
            let _ = file.metadata().unwrap();
        }),
    );
    time(
        "File::open + read_to_end（自管理）",
        Box::new(|path| {
            let mut file = std::fs::File::open(root.join(path)).unwrap();
            let mut buf = Vec::with_capacity(300);
            file.read_to_end(&mut buf).unwrap();
        }),
    );
    time(
        "fs::read（open+fstat+read）",
        Box::new(|path| {
            let _ = std::fs::read(root.join(path)).unwrap();
        }),
    );
    time(
        "read_note_in（全流程，不含分词）",
        Box::new(|path| {
            let _ = ki_vault::read_note_in(&vault, path).unwrap();
        }),
    );

    // 分词单独量：同一篇正文重复算 10k 次，把 IO 完全排除。
    let note = ki_vault::read_note_in(&vault, &paths[0]).unwrap();
    let started = Instant::now();
    for _ in 0..FILES {
        let _ = ki_search::project(&note.content);
        let _ = ki_search::token_stream(&note.content);
    }
    println!(
        "{:<34} {:>8.0}ms total  {:>7.1}µs/篇",
        "project + token_stream（纯 CPU）",
        started.elapsed().as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1e6 / FILES as f64
    );

    // ── 决定性实验：619µs 是「每个 open 的固定成本」还是「目录查找慢」？ ──
    //
    // 同一个文件 open 一万次，路径解析、目录查找、NTFS 索引全部命中同一份缓存。
    // 如果它仍然是 600µs，那这个数字就是 `CreateFileW` 在本机的单价
    // （杀软实时保护 / 文件系统过滤驱动），与我们的代码、路径深度、
    // 夹具布局全都无关——那时「10k 篇 ≤3s」在物理上就不成立。
    let one = root.join(&paths[0]);
    let started = Instant::now();
    for _ in 0..FILES {
        let _ = std::fs::File::open(&one).unwrap();
    }
    println!(
        "{:<34} {:>8.0}ms total  {:>7.1}µs/次",
        "同一个文件 open 10k 次（对照）",
        started.elapsed().as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1e6 / FILES as f64
    );

    let started = Instant::now();
    for _ in 0..FILES {
        let _ = std::fs::metadata(&one).unwrap();
    }
    println!(
        "{:<34} {:>8.0}ms total  {:>7.1}µs/次",
        "同一个文件 stat 10k 次（对照）",
        started.elapsed().as_secs_f64() * 1000.0,
        started.elapsed().as_secs_f64() * 1e6 / FILES as f64
    );

    // ── 并行 open：如果 619µs 是**等待**（过滤驱动 / 磁盘延迟），
    //    多线程重叠等待就能把墙钟时间压下来；如果它是带宽或锁，线程数
    //    加上去不会变快。这一项决定「5× 是否还有别的路」。
    for threads in [4usize, 8, 16] {
        let per = paths.len().div_ceil(threads);
        let root_ref = &root;
        let started = Instant::now();
        std::thread::scope(|scope| {
            for chunk in paths.chunks(per) {
                scope.spawn(move || {
                    for path in chunk {
                        let _ = std::fs::File::open(root_ref.join(path)).unwrap();
                    }
                });
            }
        });
        println!(
            "{:<34} {:>8.0}ms total  {:>7.1}µs/篇（墙钟）",
            format!("{threads} 线程并行 open"),
            started.elapsed().as_secs_f64() * 1000.0,
            started.elapsed().as_secs_f64() * 1e6 / paths.len() as f64
        );
    }
}

// ---------------------------------------------------------------------------
// 验收：树先出来（B-27 第 ② 步）
// ---------------------------------------------------------------------------

/// 「先出树」这条用例的夹具：1,000 篇。
///
/// 不用 10k：判据是**顺序**（树先返回、索引后建完），不是耗时。10k 会让这条
/// 每次 `cargo test` 都跑的用例多花两分钟，而它对顺序一无所知。
const SMALL_FILES: usize = 1_000;

fn small_fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-index-early")
}

fn build_small_fixture(root: &Path) {
    if root.exists() {
        fs::remove_dir_all(root).expect("清掉上一轮的夹具");
    }
    fs::create_dir_all(root).unwrap();
    for index in 0..SMALL_FILES {
        fs::write(root.join(format!("n{index:05}.md")), note_body(index)).unwrap();
    }
}

/// `vault_scan` 必须**先出树**，索引在后台建。
///
/// 三条判据，任何一条不成立都说明「先出树」没做到：
///
/// 1. 返回时索引状态还是 `Building`——这是「树没有等索引」的直接证据；
/// 2. 树本身是完整的（1,000 篇，不多不少）；
/// 3. 索引最终自己建完（`Ready` + 1,000 篇可搜），不是「后台任务根本没跑」。
///
/// 第 1 条不是时间断言：它读的是状态，而状态在 `spawn` **之前**就被置位。
/// 唯一能让它假红的情况是"1,000 篇在几微秒内建完"，那在任何磁盘上都不成立。
#[test]
fn vault_scan_returns_before_the_index_is_ready() {
    let root = small_fixture_root();
    build_small_fixture(&root);
    reset_index(&root);

    let root_string = root.to_string_lossy().into_owned();
    let started = Instant::now();
    let scan = block_on(ki_ipc::vault_scan(root_string.clone())).expect("vault_scan");
    let scan_ms = started.elapsed();

    assert_eq!(scan.total_files, SMALL_FILES, "树必须完整");
    assert!(
        scan_ms < Duration::from_millis(500),
        "1k 夹具的树不该要 {scan_ms:?}（扫描本身是毫秒级）"
    );

    let at_return = index_state_for_test(&root);
    assert!(
        matches!(at_return, IndexState::Building { .. }),
        "vault_scan 返回时索引应当还在建，实际 {at_return:?}"
    );

    // 等后台建完。
    let deadline = Instant::now() + Duration::from_secs(120);
    while index_state_for_test(&root) != IndexState::Ready {
        assert!(Instant::now() < deadline, "索引没能在 120 秒内就绪");
        std::thread::sleep(Duration::from_millis(20));
    }
    let ready_ms = started.elapsed();
    assert!(ready_ms > scan_ms, "索引就绪不可能早于树返回");

    // 就绪之后：状态是 Ready，且搜索看到的是全量索引。
    let result = block_on(ki_ipc::vault_search(
        root_string,
        "笔记 001".to_owned(),
        None,
    ))
    .expect("搜索");
    assert_eq!(result.index_state, IndexState::Ready);
    assert_eq!(result.indexed_files, SMALL_FILES);
    assert!(result.total > 0, "就绪之后必须能搜到");

    println!("树 {scan_ms:?} / 索引就绪 {ready_ms:?}（{SMALL_FILES} 篇）");
}

/// 10k 库上「树可见」与「索引就绪」两个时点。
///
/// 与 `ten_thousand_notes_index_build` 的区别：那条量的是**同步**同步的耗时
/// （`sync_after_scan`），这条量的是**生产路径**——`vault_scan` 命令本身。
/// 本刀改的就是这个命令的返回时机，所以判据必须落在它身上。
///
/// ```sh
/// cargo test -p ki-ipc --release --test index_bench tree_visible -- --ignored --nocapture
/// ```
#[test]
#[ignore = "10k 夹具 + 一次全量建库，显式跑（见文件头）"]
fn tree_visible_and_index_ready_on_ten_thousand() {
    let _fixture = fixture_lock();
    let root = fixture_root();
    if !root.exists() {
        build_fixture(&root);
    }
    reset_index(&root);

    let root_string = root.to_string_lossy().into_owned();
    let started = Instant::now();
    let scan = block_on(ki_ipc::vault_scan(root_string)).expect("vault_scan");
    let tree_ms = started.elapsed();
    assert_eq!(scan.total_files, FILES);
    assert_eq!(
        fingerprint(&scan),
        0x826b_2334_0d1e_1e45,
        "树指纹必须与基线一致"
    );
    assert!(
        tree_ms < Duration::from_millis(500),
        "10k 库的树不该要 {tree_ms:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(300);
    while index_state_for_test(&root) != IndexState::Ready {
        assert!(Instant::now() < deadline, "索引没能在 300 秒内就绪");
        std::thread::sleep(Duration::from_millis(50));
    }
    let ready_ms = started.elapsed();

    let (db, _) = ki_store::IndexDb::open(&root).expect("打开索引库");
    let counts = db.counts().expect("读计数");

    println!(
        "树可见 {:.0}ms / 索引就绪 {:.0}ms（{} 篇，索引 {} 篇 / 跳过 {}）",
        tree_ms.as_secs_f64() * 1000.0,
        ready_ms.as_secs_f64() * 1000.0,
        FILES,
        counts.ok,
        counts.skipped
    );
    assert_eq!(counts.ok, FILES, "后台建库必须建全");
}
