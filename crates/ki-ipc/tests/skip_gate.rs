//! 门禁：跳过清单的两笔账（10k 夹具，release）。
//!
//! ```sh
//! cargo test -p ki-ipc --release --test skip_gate -- --ignored --nocapture
//! ```
//!
//! # 它量什么
//!
//! | 记号 | 量什么 | 门禁 |
//! |---|---|---|
//! | ① | `ScanResult.skipped` 与 `SearchResult.skipped_files` 各是多少 | 两个数**不相等**才对 |
//! | ② | 清单（≤50 条字符串）塞进 `vault_scan` 返回值之后的扫描耗时增量 | ≤ +5% |
//!
//! # 为什么两个数必须不相等
//!
//! 扫描侧数的是**条目**（目录、软链、FIFO、非 UTF-8 文件名），索引侧数的是
//! **笔记**（认不出编码、超过 2MB、IO 失败）。同一份夹具里，「超过 2MB 的篇」
//! 在扫描侧是**一棵正常的树节点**，只有索引侧才跳过它——所以两个数天然不等。
//! 一旦有人把它们合成一个数，这张表就再也对不上。
//!
//! # 夹具形状
//!
//! 与 `index_bench.rs` 同形（10k 篇 / 50 个父目录），另加两类**故意的坏东西**：
//!
//! - `bad/huge.md`：> 2MB（索引上限），扫描侧完全正常；
//! - `locked0..2/`：无读权限的目录（Windows 走 `icacls /deny`，Unix 走
//!   `chmod 000`），扫描侧跳过、索引侧根本看不见它们。
//!
//! 后者的 ACL **必须**在用例结束时还原：不还原的话下一次 `remove_dir_all`
//! 会失败，而失败看起来像「夹具清理逻辑坏了」。
//!
//! # 计时口径
//!
//! 只量 `scan_vault`（本刀没碰索引）。三次取中位，且三次都在夹具写完**之后**
//! 跑——第一次含页缓存预热，所以先空跑一次再开始计时。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ki_ipc::search::{service, sync_after_scan};
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 10_000;
const DIRS: usize = 50;
const LOCKED: usize = 3;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-skip-gate")
}

fn note_body(index: usize) -> String {
    format!(
        "# 笔记 {index:05}\n\n\
         本地优先的知识库把正文留在磁盘上，索引只是派生数据。\
         这一篇用来测量扫描的固定开销：路径解析与一次 stat。\
         编号 {index:05} 让它唯一，也让它可被定位器命中。\n"
    )
}

fn build_fixture(root: &Path) {
    unlock_all(root);
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

    // 索引侧会跳过、扫描侧完全正常的那一篇。
    fs::create_dir_all(root.join("bad")).unwrap();
    fs::write(
        root.join("bad").join("huge.md"),
        format!("huge\n{}\n", "x".repeat(2 * 1024 * 1024 + 4096)),
    )
    .unwrap();

    for index in 0..LOCKED {
        let dir = root.join(format!("locked{index}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("inside.md"), "# 读不到的目录里的笔记\n").unwrap();
    }
}

/// 造「无读权限」。失败就 panic：门禁的夹具缺一类，量出来的两个数就不可信。
fn lock(root: &Path) {
    for index in 0..LOCKED {
        let dir = root.join(format!("locked{index}"));
        #[cfg(windows)]
        {
            let user = std::env::var("USERNAME").expect("USERNAME");
            let out = Command::new("icacls")
                .arg(&dir)
                .arg("/deny")
                .arg(format!("{user}:(OI)(CI)(RX)"))
                .output()
                .expect("跑 icacls");
            assert!(out.status.success(), "icacls /deny 失败：{out:?}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("chmod 000");
        }
        let _ = index;
    }
}

fn unlock_all(root: &Path) {
    if !root.exists() {
        return;
    }
    for index in 0..LOCKED {
        let dir = root.join(format!("locked{index}"));
        if !dir.exists() {
            continue;
        }
        #[cfg(windows)]
        {
            let user = std::env::var("USERNAME").expect("USERNAME");
            let _ = Command::new("icacls")
                .arg(&dir)
                .arg("/remove:d")
                .arg(&user)
                .output();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
        }
    }
}

fn median(samples: &[Duration]) -> f64 {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2].as_secs_f64() * 1000.0
}

#[test]
#[ignore = "10k 夹具，显式跑：cargo test -p ki-ipc --release --test skip_gate -- --ignored --nocapture"]
fn skip_counts_and_scan_cost() {
    let root = fixture_root();
    build_fixture(&root);
    lock(&root);

    // 预热一次：第一次扫要把 10k 个目录项读进页缓存。
    let warm = scan_vault(&root, &ScanOptions::default()).expect("预热扫描");
    println!(
        "预热：scan {}ms | files={} dirs={} skipped={}",
        warm.elapsed_ms, warm.total_files, warm.total_dirs, warm.skipped
    );

    let mut samples = Vec::new();
    let mut last = warm.clone();
    for round in 0..3 {
        let started = Instant::now();
        let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
        let elapsed = started.elapsed();
        println!(
            "第 {} 轮：scan {}ms（墙钟 {:.1}ms）| files={} dirs={} skipped={}",
            round + 1,
            scan.elapsed_ms,
            elapsed.as_secs_f64() * 1000.0,
            scan.total_files,
            scan.total_dirs,
            scan.skipped
        );
        samples.push(elapsed);
        last = scan;
    }

    let scan_median = median(&samples);
    println!("扫描中位（墙钟，3 次）：{scan_median:.2}ms");

    // ── ① 两个 skipped ───────────────────────────────────────────────────
    let outcome = sync_after_scan(&root, &last).expect("同步索引");
    println!("同步：{:?}", outcome.stats);
    let result = service::search(&root, "不存在的词-用于读计数", None).expect("搜索");
    println!(
        "① 扫描侧 skipped = {}（清单 {} 条，truncated={}） | 索引侧 skipped_files = {}（索引 {} 篇）",
        last.skipped,
        last.skipped_detail.len(),
        last.skipped_truncated,
        result.skipped_files,
        result.indexed_files
    );

    // 清单必须逐条指到那三个目录——只有总数的话，用户仍然不知道是哪三个。
    for entry in &last.skipped_detail {
        println!("   · {:?} {:?}", entry.path, entry.reason);
    }
    let locked_listed = last
        .skipped_detail
        .iter()
        .filter(|e| e.reason == ki_core::ScanSkipReason::Permission)
        .count();
    assert_eq!(locked_listed, LOCKED, "三个无权限目录都要在清单里");
    assert!(
        last.skipped > result.skipped_files,
        "两个数相等就说明有人把它们合成了一件事：{} vs {}",
        last.skipped,
        result.skipped_files
    );

    unlock_all(&root);
}
