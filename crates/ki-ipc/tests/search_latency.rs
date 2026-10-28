//! 搜索路径的 P95 探针（10k 夹具、release）。
//!
//! ```sh
//! cargo test -p ki-ipc --release --test search_latency -- --ignored --nocapture
//! ```
//!
//! 索引就绪之后，一次搜索的耗时不随本刀的改动变化——本刀只加了一次状态读
//! 与一个字段。这个探针是验收第 4 条（±5%）的量具，同时也用来确认
//! 「后台建索引期间搜索不排队」不是靠运气。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ki_ipc::search::{invalidate, service, sync_after_scan};
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 10_000;
const DIRS: usize = 50;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-index-bench")
}

fn build_fixture(root: &Path) {
    if root.exists() {
        std::fs::remove_dir_all(root).expect("清掉上一轮的夹具");
    }
    for dir in 0..DIRS {
        std::fs::create_dir_all(root.join(format!("d{dir:02}"))).unwrap();
    }
    for index in 0..FILES {
        let dir = format!("d{:02}", index % DIRS);
        let body = format!(
            "# 笔记 {index:05}\n\n本地优先的知识库把正文留在磁盘上，索引只是派生数据。\
             这一篇用来测量建库时每篇的固定开销：路径解析、读盘、分词、写库。\
             编号 {index:05} 让它唯一，也让它可被定位器命中。\n"
        );
        std::fs::write(root.join(dir).join(format!("n{index:05}.md")), body).unwrap();
    }
}

fn p95(samples: &[Duration]) -> f64 {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    sorted[(sorted.len() as f64 * 0.95) as usize - 1].as_secs_f64() * 1000.0
}

fn median(samples: &[Duration]) -> f64 {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2].as_secs_f64() * 1000.0
}

#[test]
#[ignore = "10k 夹具 + 全量建库，显式跑"]
fn search_latency_after_index_is_ready() {
    let root = fixture_root();
    if !root.exists() {
        build_fixture(&root);
    }

    invalidate(&root);
    let meta = root.join(ki_core::METADATA_DIR);
    if meta.exists() {
        std::fs::remove_dir_all(&meta).unwrap();
    }
    let scan = scan_vault(&root, &ScanOptions::default()).unwrap();
    let started = Instant::now();
    sync_after_scan(&root, &scan).unwrap();
    println!("建库 {:.0}ms", started.elapsed().as_secs_f64() * 1000.0);

    // 四条查询各走一条通道：中文 bigram、纯数字（ASCII 投影）、一个更长、
    // 候选更多的中文词，以及**单字**。**不要**用文件名做查询：文件名命中只
    // 影响排序，不产生命中行（`locate_all` 只数正文），拿它当探针会得到 0 条。
    //
    // `记` 是本文件里唯一一条单字查询，它钉的是 M7 第六刀：这条正文模板里
    // 每篇标题都是 `# 笔记 {n}`，所以 `记` 命中全库 10000 篇，必然走截断路径。
    // 改动前它是 `Mode::FullScan`（10k 次 open，p50 4.4 s），改动后应当落到
    // 索引通道。**它是本文件最慢的一条**：改动前跑一次要 4 分钟。
    for query in ["笔记 001", "本地优先", "00042", "记"] {
        // 预热：第一次会打开索引库（体检 + 建连接）。
        let _ = service::search(&root, query, None).unwrap();

        let mut samples = Vec::with_capacity(50);
        for _ in 0..50 {
            let t = Instant::now();
            let result = service::search(&root, query, None).unwrap();
            samples.push(t.elapsed());
            assert!(result.total > 0, "探针查询必须有命中：{query}");
        }
        println!(
            "{query:<12} p50 {:>7.2}ms  p95 {:>7.2}ms  max {:>7.2}ms",
            median(&samples),
            p95(&samples),
            samples.iter().max().unwrap().as_secs_f64() * 1000.0
        );
    }
}
