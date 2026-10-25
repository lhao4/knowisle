//! 新旧引擎对拍：**硬门槛**。
//!
//! # 为什么这条测试是这一刀的验收核心
//!
//! 换检索实现最大的风险不是"搜得慢"，是"搜得少了"——少一条命中不会报错，
//! 只会让用户某天发现"我明明写过这句话"。所以这里不是抽查几个 query，
//! 而是把旧引擎（M2 的内存子串匹配）当作**参照实现**，逐条比对。
//!
//! 对拍能成立的前提是两个引擎**共用同一个定位器**（`ki_vault::NoteLocator`）：
//! 行号、UTF-16 列号、摘要窗口是同一份代码算出来的，所以差异只可能来自
//! 候选集。这也就意味着——这里每一条断言失败，都是候选集的问题。
//!
//! # 断言的分级
//!
//! - **全扫路径**（单字 / 纯标点 / emoji）：顺序必须**完全一致**。
//!   它们不经过 FTS，候选集就是全部已索引笔记，与旧引擎等价。
//! - **索引路径**（ASCII 词 / CJK bigram）：`total` 相等、命中集合相等、
//!   每条同 key 的 `text` / `from` / `to` / `column` / `column_end` 逐字相等，
//!   并且 `name_hit` 全在正文命中之前。
//!
//! 实际上索引路径的顺序也应该一致（排序键是同一个），但**不把它写成断言**：
//! 将来若给 RRF 分数留一个排序位，顺序会变而精度不该变。把"顺序"和
//! "内容"绑在一起，会逼着下一次改动去改测试而不是改设计。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use ki_core::SearchResult;
use ki_ipc::search::{legacy, service, sync_after_scan};
use ki_vault::{scan_vault, ScanOptions};

/// 一个混排语料库：ASCII、中文、中英混排、单字、标点、emoji、子目录。
fn corpus() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("创建临时目录");
    fs::create_dir(dir.path().join("a")).expect("建子目录");
    fs::create_dir(dir.path().join("b")).expect("建子目录");

    let files: &[(&str, &str)] = &[
        (
            "a/rust-ownership.md",
            "The Rust programming language.\nOwnership and borrowing keep memory safe.\n内存安全靠所有权。\n",
        ),
        (
            "b/知识库设计.md",
            "知识库的设计与实现。\n检索索引的分词与投影是两件事。\n",
        ),
        (
            "c-mixed.md",
            "本地优先Rust实现，Rust的所有权模型。\nknowisle 是一个本地优先的库。\n",
        ),
        (
            "d-single.md",
            "的 是 一 个 常 用 字。\na b c\n标点，。！？\n",
        ),
        ("e-emoji.md", "🙂🙂 表情符号测试\n---\n"),
        ("f-检索入口.md", "这里是检索入口。\n入口只有一处。\n"),
    ];

    for (path, body) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("建父目录");
        }
        fs::write(&full, body).expect("写笔记");
    }
    dir
}

/// 建好索引的语料库。返回 `TempDir` 是为了让它活着。
fn indexed_corpus() -> tempfile::TempDir {
    let dir = corpus();
    let scan = scan_vault(dir.path(), &ScanOptions::default()).expect("扫描");
    sync_after_scan(dir.path(), &scan).expect("同步");
    dir
}

fn keys(result: &SearchResult) -> Vec<(String, u32)> {
    result
        .hits
        .iter()
        .map(|hit| (hit.path.clone(), hit.line))
        .collect()
}

/// 对拍一次查询。`strict_order` 为真时连顺序一起比。
fn assert_parity(root: &Path, query: &str, strict_order: bool) {
    let old = legacy::query(root, query, None).expect("旧引擎查询");
    let new = service::search(root, query, None).expect("新引擎查询");

    let tag = format!("[{query:?}]");
    assert_eq!(new.total, old.total, "{tag} total 不等");
    assert_eq!(
        new.indexed_files, old.indexed_files,
        "{tag} indexed_files 不等"
    );
    assert_eq!(
        new.skipped_files, old.skipped_files,
        "{tag} skipped_files 不等"
    );
    assert_eq!(new.truncated, old.truncated, "{tag} truncated 不等");
    assert_eq!(new.hits.len(), old.hits.len(), "{tag} 命中条数不等");

    // 命中集合 + 每条字段逐字相等
    let by_key: BTreeMap<(String, u32), &ki_core::SearchHit> = new
        .hits
        .iter()
        .map(|hit| ((hit.path.clone(), hit.line), hit))
        .collect();

    for o in &old.hits {
        let key = (o.path.clone(), o.line);
        let n = by_key
            .get(&key)
            .unwrap_or_else(|| panic!("{tag} 新引擎漏了 {}:{}", o.path, o.line));
        assert_eq!(n.text, o.text, "{tag} {}:{} text", o.path, o.line);
        assert_eq!(n.from, o.from, "{tag} {}:{} from", o.path, o.line);
        assert_eq!(n.to, o.to, "{tag} {}:{} to", o.path, o.line);
        assert_eq!(n.column, o.column, "{tag} {}:{} column", o.path, o.line);
        assert_eq!(
            n.column_end, o.column_end,
            "{tag} {}:{} column_end",
            o.path, o.line
        );
        assert_eq!(
            n.name_hit, o.name_hit,
            "{tag} {}:{} name_hit",
            o.path, o.line
        );
    }

    if strict_order {
        assert_eq!(keys(&new), keys(&old), "{tag} 全扫路径的顺序必须完全一致");
    }
}

/// 排序契约：文件名命中的行必须全部排在正文命中的行前面。
fn assert_name_hits_first(result: &SearchResult, tag: &str) {
    let mut seen_body = false;
    for hit in &result.hits {
        if hit.name_hit {
            assert!(!seen_body, "{tag} name_hit 排到了正文命中后面");
        } else {
            seen_body = true;
        }
    }
}

/// 索引路径（ASCII 词 + CJK bigram + 文件名通道）。
#[test]
fn indexed_queries_match_the_legacy_engine() {
    let dir = indexed_corpus();
    for query in [
        // ASCII
        "rust",
        "Rust",
        "RUST",
        "ownership",
        "knowisle",
        "the",
        // CJK bigram
        "知识库",
        "所有权",
        "分词",
        "入口",
        // 混排
        "Rust的所有权",
        "本地优先",
        // 文件名通道单独命中（正文里也有，但排序要靠 name_hit）
        "检索入口",
    ] {
        assert_parity(dir.path(), query, false);
        let new = service::search(dir.path(), query, None).expect("新引擎");
        assert_name_hits_first(&new, query);
    }
}

/// 全扫路径：单字、纯标点、emoji、空白。
///
/// 这些查询在索引里没有可查的 token（没有 bigram、没有 ASCII 词），
/// 必须逐篇字面扫描——与旧引擎**完全等价**，所以顺序也要一致。
#[test]
fn full_scan_queries_match_exactly() {
    let dir = indexed_corpus();
    for query in ["的", "a", "🙂", "🙂🙂", "---", "，。", "字。"] {
        assert_parity(dir.path(), query, true);
    }
}

/// 空查询与含换行的查询都必须返回空——旧引擎对跨行命中一律跳过。
#[test]
fn empty_and_multiline_queries_return_nothing() {
    let dir = indexed_corpus();
    for query in ["", "   ", "\t", "rust\nownership", "a\r\nb"] {
        assert_parity(dir.path(), query, true);
        let new = service::search(dir.path(), query, None).expect("新引擎");
        assert!(new.hits.is_empty(), "{query:?} 不该有命中");
        assert_eq!(new.total, 0);
    }
}

/// 随机中文子串的召回完备性。
///
/// 这条独立于对拍：对拍只证明"和旧引擎一样"，而旧引擎本身也可能漏。
/// 这里证明的是 bigram 方案的数学性质——`Q ⊆ T` 且 Q 含 ≥2 汉字时，
/// Q 的 bigram 多重集必然被 T 覆盖，所以 T 一定进候选池。
#[test]
fn recall_completeness_for_random_cjk_substrings() {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let alphabet: Vec<char> = "知识库的设计实现原语抽象岛屿边界检索索引分词投影本地优先"
        .chars()
        .collect();

    let mut rng = XorShift::new(0x1234_5678_9ABC_DEF0);
    let mut bodies = Vec::new();
    for i in 0..40 {
        let len = 12 + rng.below(30);
        let body: String = (0..len)
            .map(|_| alphabet[rng.below(alphabet.len())])
            .collect();
        fs::write(dir.path().join(format!("n{i:03}.md")), &body).expect("写笔记");
        bodies.push(body);
    }

    let scan = scan_vault(dir.path(), &ScanOptions::default()).expect("扫描");
    sync_after_scan(dir.path(), &scan).expect("同步");

    for _ in 0..200 {
        let which = rng.below(bodies.len());
        let chars: Vec<char> = bodies[which].chars().collect();
        let start = rng.below(chars.len() - 1);
        let take = 2 + rng.below(chars.len() - start - 1);
        let query: String = chars[start..start + take].iter().collect();

        let result = service::search(dir.path(), &query, None).expect("新引擎");
        assert!(
            result
                .hits
                .iter()
                .any(|hit| hit.path == format!("n{which:03}.md")),
            "漏召回：n{which:03}.md 含 {query:?}，但新引擎没命中"
        );
    }
}

/// **显式记录已知收窄**：ASCII 词中子串不再保证命中。
///
/// 旧引擎是整库子串匹配，`us` 能搜到正文里的 `Rust`；新引擎的通道 A 只做
/// 词首前缀，所以搜不到。这是用「索引体积与内存」换来的——旧引擎常驻约两倍
/// 正文，新引擎不存原文。
///
/// # 为什么拿 `c-mixed.md` 而不是 `a/rust-ownership.md`
///
/// 因为文件名通道是**字面子串**匹配（`name_lower LIKE %us%`），
/// `rust-ownership` 里就有 `us`，那篇会经文件名进池、再由定位器在正文里
/// 找到 `us`——于是收窄根本不显现。这不是漏洞：文件名通道与定位器都还是
/// 字面语义，只有**正文**的 ASCII 匹配从「子串」收窄成了「词首前缀」。
///
/// 这条断言的作用不是"验证 bug 存在"，而是**防止有人以为对拍已经覆盖了它**：
/// 对拍用例刻意没有这种词中子串查询，因为两条路径在那里必然不同。
/// 哪天补了 `trigram` 第三通道，这条测试会红，那时把它改成"必须命中"即可。
#[test]
fn known_divergence_ascii_infix_is_documented() {
    let dir = indexed_corpus();

    let old = legacy::query(dir.path(), "us", None).expect("旧引擎");
    assert!(
        old.hits.iter().any(|hit| hit.path == "c-mixed.md"),
        "旧引擎的子串匹配应当能命中 c-mixed.md 正文里 Rust 的 us"
    );

    let new = service::search(dir.path(), "us", None).expect("新引擎");
    assert!(
        !new.hits.iter().any(|hit| hit.path == "c-mixed.md"),
        "新引擎的通道 A 只做词首前缀，正文里的词中子串不保证——若这里变红，\
         说明收窄已被修复，请把本条测试改成「必须命中」并更新 ADR-007"
    );

    // 词首前缀仍然命中：`ru` / `rus` / `rust` 都能搜到。
    for query in ["ru", "rus", "rust"] {
        let hit = service::search(dir.path(), query, None).expect("新引擎");
        assert!(
            hit.hits.iter().any(|h| h.path == "c-mixed.md"),
            "{query:?} 作为词首前缀必须命中"
        );
    }
}

/// 极简 xorshift64*。只为让随机用例可复现，不引 `rand`。
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as usize
    }
}
