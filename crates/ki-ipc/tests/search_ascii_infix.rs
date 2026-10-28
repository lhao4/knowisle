//! 通道 A'（ASCII 词内 bigram）的**召回完备性**对拍：索引候选必须与全扫命中集
//! **相等**。
//!
//! # 判据为什么是「相等」
//!
//! 本刀把「单个 ≥2 字符的 ASCII 词」从词首前缀换成了子串语义。改对了是
//! 「`us` 终于能搜到 `Rust`」，改错了是**静默少结果**：索引少给一个候选，
//! `total` 就少算，`hits` 就少一条，不报错、不抛异常。
//!
//! 所以 oracle 不能用索引，也不能用同一份 `plan()`：这里拿 [`NoteLocator`]
//! 把全库逐篇字面扫一遍——那正是 `Mode::FullScan` 做的事——再按 `finish`
//! 的全序排序截断，逐元素比对。
//!
//! # 取样
//!
//! 从 1000 篇真实语料里抽 **100 个片段**（1/2/3/4 字符各 25 个），片段本身
//! 是语料里**真实出现过的子串**，不是手写常量：手写的片段会随文档改动失效，
//! 而「真实子串」保证每条都有命中，对拍不会退化成 100 次 `0 == 0`。
//!
//! 取样偏向词中：长度 L 的词有 `L-1` 个子串、只有 1 个是词首前缀，所以随机
//! 抽到的子串天然多数是「前缀够不着」的那种——那正是本刀要修的部分。
//!
//! # 为什么 1000 篇而不是 10k
//!
//! 每条片段都要跑一遍服务端搜索（最多 1000 篇定位），10k 篇 × 100 条会把
//! 用例从几十秒拖到十几分钟。召回完备性是**切分规则**的性质，与库多大无关；
//! 10k 上的体积与耗时另有 `ascii_infix_cost.rs`。

mod common;

use std::collections::HashSet;
use std::path::PathBuf;

use ki_core::SearchHit;
use ki_ipc::search::{service, sync_after_scan};
use ki_search::project;
use ki_vault::{scan_vault, NoteLocator, QueryNeedles, ScanOptions, MAX_HITS, MAX_HITS_PER_FILE};

const FILES: usize = 1000;
const DIRS: usize = 20;
/// 每种长度抽多少个片段。
const PER_LEN: usize = 25;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-single-char-1000")
}

/// 一条命中的可比形态。`SearchHit` 不 derive `PartialEq`，而这里要的是
/// **逐字段**相同，不是「条数与首条相同」。
type Shape = (String, u32, u32, u32, u32, u32, String, bool);

fn shape(hit: &SearchHit) -> Shape {
    (
        hit.path.clone(),
        hit.line,
        hit.from,
        hit.to,
        hit.column,
        hit.column_end,
        hit.text.clone(),
        hit.name_hit,
    )
}

fn shapes(hits: &[SearchHit]) -> Vec<Shape> {
    hits.iter().map(shape).collect()
}

/// 从语料里抽 100 个真实片段：1/2/3/4 字符各 [`PER_LEN`] 个。
fn sample_fragments(notes: &[(String, String)]) -> Vec<String> {
    // pool[len - 1] = 语料里出现过的、长度为 len 的不同 ASCII 子串。
    let mut pool: Vec<HashSet<String>> = (0..4).map(|_| HashSet::new()).collect();
    for (_, text) in notes {
        for word in project(text).split_whitespace() {
            let chars: Vec<char> = word.chars().collect();
            for (offset, slots) in pool.iter_mut().enumerate() {
                for window in chars.windows(offset + 1) {
                    slots.insert(window.iter().collect());
                }
            }
        }
    }

    let mut rng = common::XorShift::new(0xA5C1_1F17_0B26_0001);
    let mut out = Vec::with_capacity(PER_LEN * 4);
    for (offset, slots) in pool.iter().enumerate() {
        let len = offset + 1;
        // 排序之后抽：`HashSet` 的迭代顺序不稳定，不排序的话同一份语料
        // 每次跑抽到的片段都不一样，「失败可复现」就没了。
        let mut items: Vec<String> = slots.iter().cloned().collect();
        items.sort();
        assert!(
            items.len() >= PER_LEN,
            "长度 {len} 的候选只有 {} 个，语料不够",
            items.len()
        );
        for _ in 0..PER_LEN {
            let at = rng.below(items.len());
            out.push(items.swap_remove(at));
        }
    }
    out
}

/// 片段作为**词首前缀**出现的篇数——也就是改前那条通道能命中的上限。
fn prefix_face(lower: &[String], fragment: &str) -> usize {
    lower
        .iter()
        .filter(|text| {
            let bytes = text.as_bytes();
            let mut from = 0;
            while let Some(pos) = text[from..].find(fragment) {
                let at = from + pos;
                if at == 0 || !bytes[at - 1].is_ascii_alphanumeric() {
                    return true;
                }
                from = at + 1;
                if from >= text.len() {
                    break;
                }
            }
            false
        })
        .count()
}

/// 片段作为**字面子串**出现的篇数——本刀要求 `total` 与它相等（按行算）。
fn substring_face(lower: &[String], fragment: &str) -> usize {
    lower.iter().filter(|text| text.contains(fragment)).count()
}

#[test]
fn ascii_infix_candidates_equal_full_scan_hits() {
    let root = fixture_root();
    common::ensure_fixture(&root, FILES, DIRS);

    // 每次重建索引：判据是**切分规则**，不是 `size + mtime`。复用上一轮的库
    // 会让变异验证（改规则、不改 `TOKENIZER_VERSION`）跑出假绿或假红。
    common::reset_index(&root);
    let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(&root, &scan).expect("同步");

    let notes = common::load_notes(&root);
    assert_eq!(notes.len(), FILES, "夹具口径：正好 {FILES} 篇");
    let locators: Vec<NoteLocator> = notes
        .iter()
        .map(|(rel, content)| NoteLocator::new(rel.clone(), content.clone()))
        .collect();

    let fragments = sample_fragments(&notes);
    // 小写正文只算一次：100 个片段 × 1000 篇，逐条 `to_lowercase` 会白分配 240 MB。
    let lower: Vec<String> = notes.iter().map(|(_, text)| text.to_lowercase()).collect();

    let mut lost = 0usize;
    let mut narrowed = 0usize;
    for fragment in &fragments {
        let needles = QueryNeedles::new(fragment);

        // 独立口径：全库逐篇定位。这就是 `Mode::FullScan` 的全部内容。
        let mut oracle_hits: Vec<SearchHit> = Vec::new();
        let mut oracle_lines = 0usize;
        for locator in &locators {
            let located = locator.locate(&needles, MAX_HITS_PER_FILE);
            oracle_lines += located.lines;
            oracle_hits.extend(located.hits);
        }
        // 排序键与 `finish` 逐字相同：name_hit 降序 → path 升序 → line 升序。
        oracle_hits.sort_by(|a, b| {
            b.name_hit
                .cmp(&a.name_hit)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        oracle_hits.truncate(MAX_HITS);

        let result = service::search(&root, fragment, None).expect("搜索");

        let sub = substring_face(&lower, fragment);
        let pre = prefix_face(&lower, fragment);
        if sub > 0 && pre == 0 {
            lost += 1;
        } else if sub > pre {
            narrowed += 1;
        }

        assert_eq!(
            result.total, oracle_lines,
            "{fragment:?} 的 total 少了：索引通道漏了候选（真实命中 {oracle_lines} 行，\
             子串面 {sub} 篇 / 词首面 {pre} 篇）"
        );
        assert!(
            !result.truncated,
            "{fragment:?} 在 1000 篇夹具上不该截断（子串面 {sub} 篇 ≤ 1200）"
        );
        assert_eq!(
            shapes(&result.hits),
            shapes(&oracle_hits),
            "{fragment:?} 的 hits 不是逐元素相同"
        );
    }

    println!(
        "对拍 {} 个片段（1/2/3/4 字符各 {PER_LEN}）：{} 个改前完全搜不到，\
         {} 个改前只搜到一部分",
        fragments.len(),
        lost,
        narrowed
    );
}

/// **那个经典例子**：正文含 `Rust`、文件名不含 `us`，`us` 必须命中它。
///
/// 文件名刻意取 `ownership.md`——它不含 `us`。若叫 `rust.md`，文件名通道的
/// `LIKE %us%` 会先把这篇捞进来，用例就永远绿，证明不了通道 A' 存在。
#[test]
fn b26_example_us_finds_rust_in_the_body() {
    let dir = tempfile::tempdir().expect("创建临时目录");
    std::fs::write(
        dir.path().join("ownership.md"),
        "Rust 的 Ownership 所有权模型。\n本地优先的知识库。\n",
    )
    .expect("写笔记");
    std::fs::write(dir.path().join("plain.md"), "只有中文的一篇。\n").expect("写笔记");

    let scan = scan_vault(dir.path(), &ScanOptions::default()).expect("扫描");
    sync_after_scan(dir.path(), &scan).expect("同步");

    for query in ["us", "ru", "rus", "rust", "owner", "ship"] {
        let result = service::search(dir.path(), query, None).expect("搜索");
        assert_eq!(
            result.total, 1,
            "{query:?} 必须命中 ownership.md 正文里的 Rust（文件名不含 us）"
        );
        assert_eq!(result.hits[0].path, "ownership.md", "{query:?}");
        assert!(!result.truncated, "{query:?}");
    }

    // 反面：`us` 不许因为「模糊」而命中不含它的那篇。
    let other = service::search(dir.path(), "zz", None).expect("搜索");
    assert_eq!(other.total, 0);
}
