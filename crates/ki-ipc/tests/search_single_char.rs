//! 单字查询的**召回完备性**对拍：索引通道的候选必须与全扫命中集**相等**。
//!
//! # 为什么这条用例是这一刀的门禁
//!
//! 本刀把单字查询从「逐篇全扫」改成「索引前缀 `"c"*` + 定位器」。改对了是
//! 100 倍，改错了是**静默少结果**：索引少给一个候选，`total` 就少算，`hits`
//! 就少一条，而它不报错、不抛异常，只是「搜不到」。
//!
//! 所以判据只能是**相等**，不是包含、不是「大部分相同」。用例拿一份独立口径
//! 把全库扫一遍：测试自己构造 [`NoteLocator`] 逐篇定位——那正是
//! `Mode::FullScan` 做的事——再按 `finish` 的全序排序截断，逐元素比对。
//!
//! # 夹具与取样
//!
//! 1000 篇真实语料（`tests/common`，约 800 个不同汉字）。取样分三段：
//!
//! 1. 命中面最大的 5 个字——保证「候选接近池子预算」这一档被测到；
//! 2. 夹具里出现过的字里随机抽 155 个——这是覆盖洞真正会暴露的地方
//!    （run 末字符后面跟空格 / 换行 / 标点）；
//! 3. CJK 基本区里均匀抽到 200 个为止——大多数在夹具里根本不出现，
//!    它们必须给出 0 命中而不是随便给点什么。
//!
//! # 为什么 1000 篇而不是 10k
//!
//! 每个字都要跑一遍全扫，10k 篇 × 200 字 = 200 万次定位，用例会从十几秒
//! 变成十几分钟。而覆盖洞是**切分规则**的性质，与库多大无关——1000 篇里
//! 有 800 个不同的字，够把三类洞（行尾、空白、标点）都撞上。10k 上的
//! 性能另有 `single_char_latency.rs`。

mod common;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ki_core::SearchHit;
use ki_ipc::search::{service, sync_after_scan};
use ki_vault::{scan_vault, NoteLocator, QueryNeedles, ScanOptions, MAX_HITS, MAX_HITS_PER_FILE};

const FILES: usize = 1000;
const DIRS: usize = 20;
/// 取样总数。
const SAMPLES: usize = 200;
/// 其中命中面最大的几个字：保证池子接近 `POOL_EXACT_MAX` 的那一档被测到。
const TOP_FACE: usize = 5;
/// 其中从夹具字符集里抽多少个。
const FROM_FIXTURE: usize = 160;

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

#[test]
fn indexed_single_char_candidates_equal_full_scan_hits() {
    let root = fixture_root();
    common::ensure_fixture(&root, FILES, DIRS);

    // 每次重建索引：判据是**切分规则**，不是 `size + mtime`。复用上一轮的库
    // 会让变异验证（改规则、不改 `TOKENIZER_VERSION`）跑出假绿或假红。
    common::reset_index(&root);
    let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(&root, &scan).expect("同步");

    // 全库正文读进内存，定位器只构造一次：oracle 要跑 200 遍全扫，
    // 每遍重新读盘会把这条用例从十几秒拖到几分钟。
    let notes = common::load_notes(&root);
    assert_eq!(notes.len(), FILES, "夹具口径：正好 {FILES} 篇");
    let locators: Vec<NoteLocator> = notes
        .iter()
        .map(|(rel, content)| NoteLocator::new(rel.clone(), content.clone()))
        .collect();

    // ---- 取样 ----
    let mut face: HashMap<char, usize> = HashMap::new();
    for (_, content) in &notes {
        let mut seen = HashSet::new();
        for c in content.chars().filter(|c| ki_search::is_cjk(*c)) {
            if seen.insert(c) {
                *face.entry(c).or_default() += 1;
            }
        }
    }
    let mut by_face: Vec<(char, usize)> = face.iter().map(|(c, n)| (*c, *n)).collect();
    by_face.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let distinct: Vec<char> = {
        let mut d: Vec<char> = face.keys().copied().collect();
        d.sort_unstable();
        d
    };

    let mut rng = common::XorShift::new(0x51A7_C0DE_1234_5678);
    let mut picked: Vec<char> = by_face.iter().take(TOP_FACE).map(|(c, _)| *c).collect();
    let mut seen: HashSet<char> = picked.iter().copied().collect();

    let from_fixture = FROM_FIXTURE.min(distinct.len());
    while picked.len() < from_fixture {
        let c = distinct[rng.below(distinct.len())];
        if seen.insert(c) {
            picked.push(c);
        }
    }
    // 剩下的从 CJK 基本区均匀抽：绝大多数在夹具里根本不出现。
    while picked.len() < SAMPLES {
        let c = char::from_u32(0x4E00 + rng.below(0x9FFF - 0x4E00 + 1) as u32).unwrap();
        if seen.insert(c) {
            picked.push(c);
        }
    }

    // ---- 逐字对拍 ----
    let mut worst = 0usize;
    let mut nonempty = 0usize;
    for c in &picked {
        let query = c.to_string();
        let needles = QueryNeedles::new(&query);

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

        let result = service::search(&root, &query, None).expect("搜索");
        let candidates = face.get(c).copied().unwrap_or(0);
        worst = worst.max(candidates);
        if oracle_lines > 0 {
            nonempty += 1;
        }

        assert_eq!(
            result.total, oracle_lines,
            "{query:?} 的 total 少了：索引通道漏了候选（真实命中 {oracle_lines} 行，候选 {candidates} 篇）"
        );
        assert!(
            !result.truncated,
            "{query:?} 在 1000 篇夹具上不该截断（候选 {candidates} 篇 ≤ 1200）"
        );
        assert_eq!(
            shapes(&result.hits),
            shapes(&oracle_hits),
            "{query:?} 的 hits 不是逐元素相同"
        );
    }

    println!(
        "对拍 {} 个字（{} 个有命中），单字候选最大 {} 篇，全部与全扫命中集相等",
        picked.len(),
        nonempty,
        worst
    );
}
