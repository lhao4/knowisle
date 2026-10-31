//! 门禁：显示序的代价与差异（10k 夹具，release）。
//!
//! ```sh
//! cargo test -p ki-vault --release --test sort_gate -- --ignored --nocapture
//! ```
//!
//! # 它量什么
//!
//! | 记号 | 量什么 | 门禁 |
//! |---|---|---|
//! | ① | `scan_vault` 的 p50（31 次取中位），基线 vs 预计算键 | ≤ +20% |
//! | ② | 新树序与旧实现的**差异条数**，按名字形状分类 | 逐条能解释 |
//!
//! # 为什么基线要换回旧代码跑
//!
//! 「把旧比较器抄进用例里再排一遍」量不到 `scan_vault` 的接线成本
//! （构造排序器、生成 10k 个键）。所以基线是**真的换回旧实现**跑同一份夹具：
//! 把 `src/scan.rs` 换成改动前的版本、重跑本用例、再换回来。
//!
//! # 夹具形状
//!
//! 10k 篇 / 50 个目录（45 个 ASCII + 5 个中文），名字按 `index % 10` 分四种：
//! 纯编号、中文姓氏 + 编号、小写 `note{n}`、大写 `Note{n}`。四种形状分别对应
//! 「数字感知会变」「拼音会变」「大小写兜底」「两者都不变」四类差异。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ki_core::FileNode;
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 10_000;
const ASCII_DIRS: usize = 45;
const CJK_DIRS: &[&str] = &["笔记", "归档", "项目", "读书", "日常"];
const RUNS: usize = 31;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-sort-gate")
}

fn note_body(index: usize) -> String {
    format!("# 笔记 {index:05}\n\n排序门禁的夹具。编号 {index:05}。\n")
}

fn note_name(index: usize) -> String {
    match index % 10 {
        0..=5 => format!("n{index:05}.md"),
        6 | 7 => {
            const SURNAMES: &[&str] = &["张", "李", "王", "阿", "陈"];
            format!("{}{index:03}.md", SURNAMES[index % SURNAMES.len()])
        }
        8 => format!("note{index}.md"),
        _ => format!("Note{index}.md"),
    }
}

fn build_fixture(root: &Path) {
    if root.exists() {
        fs::remove_dir_all(root).expect("清掉上一轮的夹具");
    }
    let mut dirs: Vec<String> = (0..ASCII_DIRS).map(|d| format!("d{d:02}")).collect();
    dirs.extend(CJK_DIRS.iter().map(|d| (*d).to_owned()));
    for dir in &dirs {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    for index in 0..FILES {
        let dir = &dirs[index % dirs.len()];
        fs::write(root.join(dir).join(note_name(index)), note_body(index)).unwrap();
    }
}

fn p50(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// 旧实现的比较器（改动前 `scan.rs::compare_entries` 的逐字复制）。
fn old_order(children: &[FileNode]) -> Vec<String> {
    let mut names: Vec<&FileNode> = children.iter().collect();
    names.sort_by(|a, b| {
        b.kind
            .is_dir()
            .cmp(&a.kind.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    names.into_iter().map(|node| node.name.clone()).collect()
}

#[derive(Default)]
struct Diff {
    /// 位置不同（同一层里第 i 个名字变了）。
    moved: usize,
    /// 含中文的层里发生的位移。
    cjk: usize,
    /// 纯 ASCII、含数字的层里发生的位移。
    digits: usize,
    /// 纯 ASCII、不含数字的层里发生的位移——**这一格必须是 0**。
    plain: usize,
}

fn walk_diff(node: &FileNode, diff: &mut Diff) {
    let children = node.children.as_deref().unwrap_or(&[]);
    if !children.is_empty() {
        let actual: Vec<String> = children.iter().map(|c| c.name.clone()).collect();
        let expected = old_order(children);
        let has_cjk = actual.iter().any(|name| !name.is_ascii());
        let has_digit = actual.iter().any(|name| name.chars().any(|c| c.is_ascii_digit()));
        for (a, b) in actual.iter().zip(expected.iter()) {
            if a != b {
                diff.moved += 1;
                if has_cjk {
                    diff.cjk += 1;
                } else if has_digit {
                    diff.digits += 1;
                } else {
                    diff.plain += 1;
                }
            }
        }
    }
    for child in children {
        if child.kind.is_dir() {
            walk_diff(child, diff);
        }
    }
}

#[test]
#[ignore = "10k 夹具，release 手动跑（门禁③）"]
fn sort_gate() {
    let root = fixture_root();
    build_fixture(&root);
    let opts = ScanOptions::default();

    // 空跑一次：第一次含页缓存预热，不该进样本。
    let warm = scan_vault(&root, &opts).expect("预热扫描失败");
    drop(warm);

    let mut samples = Vec::with_capacity(RUNS);
    let mut last = None;
    for _ in 0..RUNS {
        let started = Instant::now();
        let result = scan_vault(&root, &opts).expect("扫描失败");
        samples.push(started.elapsed());
        last = Some(result);
    }
    let result = last.expect("至少跑过一次");
    let median = p50(&mut samples);

    let mut diff = Diff::default();
    walk_diff(&result.tree, &mut diff);

    // 键生成单独计时：它是「预计算」这条决策的全部成本，比较阶段是 memcmp。
    let mut names: Vec<&str> = Vec::with_capacity(FILES);
    collect_names(&result.tree, &mut names);
    let sorter = ki_vault::sort::Sorter::new();
    let started = Instant::now();
    let keys: Vec<ki_vault::sort::SortKey> = names.iter().map(|name| sorter.key(name)).collect();
    let key_time = started.elapsed();

    println!("── sort_gate ──────────────────────────────");
    println!("条目：{} 文件 / {} 目录", result.total_files, result.total_dirs);
    println!("scan_vault 样本：{samples:?}");
    println!("scan_vault p50：{median:?}");
    println!("键生成（{} 个名字）：{key_time:?}，键总字节 {}", keys.len(), keys.iter().map(|k| k.as_bytes().len()).sum::<usize>());
    println!(
        "与旧实现的位移：{}（中文层 {} / ASCII+数字层 {} / 纯 ASCII 层 {}）",
        diff.moved, diff.cjk, diff.digits, diff.plain
    );

    assert_eq!(
        diff.plain, 0,
        "纯 ASCII 无数字的名字不该因为换排序器而变序"
    );
    assert!(diff.moved > 0, "换了排序器却一条都没变，说明夹具没覆盖到");
}

/// 收集树里所有条目的名字（含目录），给键生成计时用。
fn collect_names<'a>(node: &'a FileNode, out: &mut Vec<&'a str>) {
    for child in node.children.as_deref().unwrap_or(&[]) {
        out.push(&child.name);
        if child.kind.is_dir() {
            collect_names(child, out);
        }
    }
}
