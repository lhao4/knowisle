//! 通道 A' 的**成本与缺口**基准（10k 真实语料夹具、release）。
//!
//! ```sh
//! cargo test -p ki-ipc --release --test ascii_infix_cost -- --ignored --nocapture
//! ```
//!
//! # 它回答三个问题
//!
//! 1. **ASCII 在真实中文技术写作里占多少**：`project` 出来的 token 数 /
//!    全部 token 数。这个比例决定「按字符展开」的通道能不能负担——中文库
//!    里 ASCII 通常只占个位数百分比，但那是猜的，这里量。
//! 2. **缺口面有多大**：25 个真实片段的「子串篇数」与「词首篇数」。
//!    前者是本刀要修的，后者是改前能命中的上限；两者的差就是缺口。
//! 3. **索引代价**：10k 建库时间（median of 3）与 `index.sqlite` 体积。
//!
//! # 为什么这些数字要能被复现
//!
//! 上一刀的教训是「预期接近 0，实测 +15.1%」。成本模型一旦只活在 ADR 的表格里，
//! 下一个人就只能靠回忆判断「这次会不会又超」。夹具与判据都住在这里，
//! 改完代码重跑一遍就能对账。
//!
//! # 它**不**断言耗时
//!
//! 墙钟在 CI 上必假红。数字进 ADR，判据留给 `search_ascii_infix.rs`。

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ki_ipc::search::{invalidate, service, sync_after_scan};
use ki_search::{project, tokens};
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 10_000;
const DIRS: usize = 50;
/// 建库重复次数，取 median。
const ROUNDS: usize = 3;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-single-char")
}

/// 两条用例共用同一份夹具，而 `cargo test -- --ignored` 会**并行**跑它们。
/// 没有这把锁的话，一条在 `reset_index` 删库，另一条正在查——实测报
/// `unable to open database file`，看起来像实现坏了。
static FIXTURE: Mutex<()> = Mutex::new(());

fn fixture_lock() -> MutexGuard<'static, ()> {
    FIXTURE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn median(samples: &[Duration]) -> f64 {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2].as_secs_f64() * 1000.0
}

/// 片段作为字面子串 / 词首前缀出现的篇数。
fn faces(lower: &[String], fragment: &str) -> (usize, usize) {
    let mut sub = 0;
    let mut pre = 0;
    for text in lower {
        if !text.contains(fragment) {
            continue;
        }
        sub += 1;
        let bytes = text.as_bytes();
        let mut from = 0;
        while let Some(pos) = text[from..].find(fragment) {
            let at = from + pos;
            if at == 0 || !bytes[at - 1].is_ascii_alphanumeric() {
                pre += 1;
                break;
            }
            from = at + 1;
            if from >= text.len() {
                break;
            }
        }
    }
    (sub, pre)
}

fn index_bytes(root: &Path) -> (u64, u64) {
    let dir = root.join(ki_core::METADATA_DIR);
    let main = std::fs::metadata(dir.join("index.sqlite"))
        .map(|m| m.len())
        .unwrap_or(0);
    let wal = std::fs::metadata(dir.join("index.sqlite-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    (main, wal)
}

#[test]
#[ignore = "10k 夹具 + 全量建库，显式跑"]
fn ascii_infix_cost() {
    let _fixture = fixture_lock();
    let root = fixture_root();
    common::ensure_fixture(&root, FILES, DIRS);
    let notes = common::load_notes(&root);
    assert_eq!(notes.len(), FILES);

    // ---- ① ASCII 体量 ----
    let mut ascii_tokens = 0usize;
    let mut cjk_tokens = 0usize;
    for (_, text) in &notes {
        ascii_tokens += project(text).split_whitespace().count();
        cjk_tokens += tokens(text).len();
    }
    let total = ascii_tokens + cjk_tokens;
    println!(
        "ASCII token {ascii_tokens} / 全部 {total} = {:.2}%（CJK {cjk_tokens}）",
        ascii_tokens as f64 * 100.0 / total as f64
    );

    // ---- ② 缺口面 ----
    let lower: Vec<String> = notes.iter().map(|(_, t)| t.to_lowercase()).collect();
    let fragments: &[&str] = &[
        "a", "e", "r", "s", "t", "i", // 1 字符
        "us", "ow", "ru", "st", "ap", "ql", // 2 字符
        "rus", "own", "api", "sql", "ate", "ter", // 3 字符
        "rust", "sqli", "lite", "tion", "ters", "ship", "sqlite", // 4+ 字符
    ];
    println!("\n片段\t长度\t子串篇数\t词首篇数\t差");
    let mut lost = 0;
    for fragment in fragments {
        let (sub, pre) = faces(&lower, fragment);
        if sub > 0 && pre == 0 {
            lost += 1;
        }
        println!(
            "{fragment}\t{}\t{sub}\t{pre}\t{}",
            fragment.chars().count(),
            sub as i64 - pre as i64
        );
    }
    println!(
        "全损片段 {lost} / {}（{:.0}%）：作为子串存在、但作为词首前缀不存在",
        fragments.len(),
        lost as f64 * 100.0 / fragments.len() as f64
    );

    // ---- ③ 建库时间与体积 ----
    let mut syncs: Vec<Duration> = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        common::reset_index(&root);
        let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
        let started = Instant::now();
        sync_after_scan(&root, &scan).expect("同步");
        let elapsed = started.elapsed();
        syncs.push(elapsed);
        println!("建库[{round}] {:.0}ms", elapsed.as_secs_f64() * 1000.0);
    }
    // 关掉连接让 WAL 落盘，再量文件。
    invalidate(&root);
    let (main, wal) = index_bytes(&root);
    println!(
        "\n建库 median {:.0}ms；index.sqlite {main} B（{:.2} MiB）+ wal {wal} B",
        median(&syncs),
        main as f64 / 1048576.0
    );

    // 顺手确认搜索仍然工作（避免「基准跑完但索引是空的」这种假绿）。
    let result = service::search(&root, "us", None).expect("搜索");
    println!(
        "对照查询 `us`：total {} / truncated {}",
        result.total, result.truncated
    );
}

/// 第七刀的**延迟账单**：候选面涨了之后，精确路径的 p50 到底是多少。
///
/// ```sh
/// cargo test -p ki-ipc --release --test ascii_infix_cost -- --ignored --nocapture latency_bill
/// ```
///
/// # 为什么这笔账要单独记
///
/// 通道 A' 把 `us` / `own` / `ru` / `ow` 这类词中子串从「全扫或漏召回」
/// 变成「索引命中」，代价是**候选面暴涨**：`us` 298 → 1163 篇、`own`
/// 77 → 890 篇。而精确路径（`total` 要准）必须逐篇读盘核对命中行，
/// 所以这不是免费的。
///
/// 上一刀报了「候选面变大」，没报「一次查询要多久」——那是不完整的账。
/// 这条用例把四个查询的候选面与 p50 打在同一行里，下一个人要判断
/// 「还能不能再宽一点」时，看的就是这张表。
#[test]
#[ignore = "10k 夹具 + 全量建库，显式跑"]
fn exact_path_latency_bill() {
    let _fixture = fixture_lock();
    let root = fixture_root();
    common::ensure_fixture(&root, FILES, DIRS);
    common::reset_index(&root);
    let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(&root, &scan).expect("同步");

    let notes = common::load_notes(&root);
    let lower: Vec<String> = notes.iter().map(|(_, t)| t.to_lowercase()).collect();

    println!("\n查询  候选篇数  total  truncated      p50      max");
    for query in ["us", "own", "ru", "ow"] {
        let (face, _) = faces(&lower, query);
        // 预热：第一次会打开索引库（体检 + 建连接）。
        let warm = service::search(&root, query, None).expect("搜索");

        let mut samples = Vec::with_capacity(10);
        for _ in 0..10 {
            let started = Instant::now();
            let result = service::search(&root, query, None).expect("搜索");
            samples.push(started.elapsed());
            assert_eq!(result.total, warm.total, "{query} 的 total 在采样期间变了");
        }
        println!(
            "{query:<6} {face:>8}  {:>6}  {:>9}  {:>7.1}ms  {:>7.1}ms",
            warm.total,
            warm.truncated,
            median(&samples),
            samples.iter().max().unwrap().as_secs_f64() * 1000.0
        );
    }
}

/// 逐查询指纹：用来做**改前 / 改后**的对账（本文件里只打印，不断言）。
///
/// ```sh
/// cargo test -p ki-ipc --release --test ascii_infix_cost -- --ignored --nocapture fingerprint
/// ```
///
/// 判据（写进 ADR）：
/// - `rust` / `rus` / `rust-ownership` 这类**词首前缀**查询，改前改后指纹必须
///   **逐字节相同**——它们本来就能命中，本刀不该动它们；
/// - `us` / `ow` / `own` 这类词中子串查询，改后指纹必须变，且 `total` 等于
///   字面子串的真实行数。
///
/// 拿同一份夹具、同一份代码跑两遍指纹相同，说明这个判据本身是稳的。
#[test]
#[ignore = "10k 夹具，显式跑"]
fn query_fingerprints() {
    let _fixture = fixture_lock();
    let root = fixture_root();
    common::ensure_fixture(&root, FILES, DIRS);
    common::reset_index(&root);
    let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(&root, &scan).expect("同步");

    let queries = [
        "ru",
        "rus",
        "rust",
        "rust-ownership",
        "own",
        "ownership",
        "us",
        "ow",
        "sqlite",
        "api",
    ];
    for query in queries {
        let result = service::search(&root, query, None).expect("搜索");
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for hit in &result.hits {
            eat(hit.path.as_bytes());
            eat(&hit.line.to_le_bytes());
            eat(&hit.from.to_le_bytes());
            eat(&hit.to.to_le_bytes());
            eat(&hit.column.to_le_bytes());
            eat(&hit.column_end.to_le_bytes());
            eat(&[u8::from(hit.name_hit)]);
        }
        println!(
            "{query:<18} total {:>6}  truncated {:>5}  hits {:>3}  fp {hash:016x}",
            result.total,
            result.truncated,
            result.hits.len()
        );
    }
}
