//! 单字查询的 p50（10k **真实语料**夹具、release）。
//!
//! ```sh
//! cargo test -p ki-ipc --release --test single_char_latency -- --ignored --nocapture
//! ```
//!
//! # 这个探针回答什么
//!
//! 1. 10k 库上单字的**命中面分布**：多少字落在 0 / 1–50 / 51–1200 /
//!    1201–9999 / 全库。这是「池子预算（`POOL_EXACT_MAX = 1200`）能不能兑现」
//!    的唯一依据——落在 1200 以上的字，`total` 会降级成下界。
//! 2. 三个代表字的 p50：命中全库（必然截断）、中间量级、命中 ≤50 篇。
//!
//! 夹具由 `tests/common` 生成（真实语料，见那里的说明）。
//!
//! 它**不**断言耗时。墙钟断言在 CI 上必假红，耗时数字进 ADR。

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ki_ipc::search::{service, sync_after_scan};
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 10_000;
const DIRS: usize = 50;
/// 每个查询量多少次取中位数。单字在改动前是 4.4 s 一次，20 次已经要 90 秒；
/// 改动后是几十毫秒，20 次的抖动同样够小。**两边用同一个数**才能对拍。
const SAMPLES: usize = 20;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-single-char")
}

/// 「含这个字的篇数」——**按篇**去重，不是按出现次数。
///
/// 这是候选数的上界（索引给的候选只多不少：文件名命中也进池），也是
/// `POOL_EXACT_MAX` 该不该生效的判据。
fn hit_face(root: &Path) -> HashMap<char, usize> {
    let mut counts: HashMap<char, usize> = HashMap::new();
    for (_, text) in common::load_notes(root) {
        let mut seen = std::collections::HashSet::new();
        for c in text.chars().filter(|c| ki_search::is_cjk(*c)) {
            if seen.insert(c) {
                *counts.entry(c).or_default() += 1;
            }
        }
    }
    counts
}

fn bucket(n: usize) -> usize {
    match n {
        0 => 0,
        1..=50 => 1,
        51..=1200 => 2,
        1201..=9999 => 3,
        _ => 4,
    }
}

fn print_distribution(label: &str, faces: &[(char, usize)]) {
    let mut buckets = [0usize; 5];
    for &(_, n) in faces {
        buckets[bucket(n)] += 1;
    }
    println!(
        "  {label}: 0 篇 {} / 1–50 篇 {} / 51–1200 篇 {} / 1201–9999 篇 {} / 全库 {}",
        buckets[0], buckets[1], buckets[2], buckets[3], buckets[4]
    );
}

fn nearest(faces: &[(char, usize)], target: usize) -> (char, usize) {
    *faces
        .iter()
        .min_by_key(|(_, n)| (*n as i64 - target as i64).abs())
        .expect("夹具非空")
}

fn p50(samples: &[Duration]) -> f64 {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2].as_secs_f64() * 1000.0
}

/// 量一个查询：p50 + `total` + `truncated`。
fn measure(root: &Path, query: &str) -> (f64, usize, bool) {
    // 预热：第一次会打开索引库（体检 + 建连接）。
    let warm = service::search(root, query, None).expect("搜索");
    assert!(warm.total > 0, "探针查询必须有命中：{query}");

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let result = service::search(root, query, None).expect("搜索");
        samples.push(started.elapsed());
        assert_eq!(result.total, warm.total, "{query} 的 total 在采样期间变了");
    }
    (p50(&samples), warm.total, warm.truncated)
}

#[test]
#[ignore = "10k 夹具 + 全量建库，显式跑"]
fn single_char_p50_after_index_is_ready() {
    let root = fixture_root();
    common::ensure_fixture(&root, FILES, DIRS);

    // 每次重建索引，理由同 `search_single_char`：判据是切分规则，不是
    // `size + mtime`。复用上一轮的库会让「改前/改后」的对比混进旧 token。
    common::reset_index(&root);
    let scan = scan_vault(&root, &ScanOptions::default()).unwrap();
    let started = Instant::now();
    sync_after_scan(&root, &scan).unwrap();
    println!("建库 {:.0}ms", started.elapsed().as_secs_f64() * 1000.0);

    // ---- 命中面分布 ----
    let face = hit_face(&root);
    let mut all: Vec<(char, usize)> = face.iter().map(|(c, n)| (*c, *n)).collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!("\n命中面（含这个字的篇数，按篇去重）：{} 个不同汉字", all.len());

    let common_chars: Vec<char> =
        "的一是不了人我在有他这为之大来以个中上们到说国和地也子时道出而要于就下得可你年生"
            .chars()
            .take(50)
            .collect();
    let common_faces: Vec<(char, usize)> = common_chars
        .iter()
        .map(|c| (*c, face.get(c).copied().unwrap_or(0)))
        .collect();
    print_distribution("50 常见字", &common_faces);

    let mut rng = common::XorShift::new(0xC0FF_EE00_1234_5678);
    let random_faces: Vec<(char, usize)> = (0..50)
        .map(|_| {
            let c = char::from_u32(0x4E00 + rng.below(0x9FFF - 0x4E00 + 1) as u32).unwrap();
            (c, face.get(&c).copied().unwrap_or(0))
        })
        .collect();
    print_distribution("50 随机字（CJK 基本区均匀）", &random_faces);
    print_distribution("夹具全部汉字", &all);

    // ---- 三个代表字 ----
    let full = all[0];
    let middle = nearest(&all, 700);
    let small = nearest(&all, 30);
    println!(
        "\n挑三个字：全库 {:?} / 中间 {:?} / 小命中面 {:?}",
        full, middle, small
    );

    println!("\n查询     命中面    total  truncated      p50");
    for (c, n) in [full, middle, small] {
        let (ms, total, truncated) = measure(&root, &c.to_string());
        println!("{c}        {n:>5}  {total:>7}  {truncated:>9}  {ms:>8.2}ms");
    }
}
