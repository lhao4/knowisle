//! 并行定位的**线程数无关**对拍：1/2/3/4/8 线程下 `hits` 逐元素相同。
//!
//! # 这条用例要证明什么
//!
//! `locate_all` 把候选池切成跨步子序列交给 `threads` 个 worker，各攒各的命中，
//! 最后统一交给 `finish` 排序。于是有三个新风险，都是「结果看着对，其实变了」：
//!
//! 1. **顺序**：worker 的完成顺序是调度决定的。若谁在并行阶段截断或直接
//!    push 到共享 `hits`，`truncate(200)` 之后留下的是哪 200 条就变成随机的。
//!    判据只能是「逐元素相同」，不是「条数相同」。
//! 2. **计数**：`lines`（`total` 用它）与 `hits`（每篇最多 3 条）是两件事。
//!    夹具因此每篇写 5 次命中：`hits` 被 `MAX_HITS_PER_FILE` 截到 3，
//!    `lines` 必须是 5。
//! 3. **错误传播**：某篇读不到时 `total` 降级为下界。这个 `bool` 要跨线程
//!    `|=` 汇总——串行实现里它只是一个局部变量，并行之后它有了第二种失效方式
//!    （某个 worker 的失败被丢掉）。
//!
//! # 为什么还要一条独立口径
//!
//! 上面三条都只证明「各线程之间一致」。四个线程一起算错也是「一致」的。
//! 所以 `total` 另外用一份**不经过定位器、也不经过索引**的逐行 AND 算法
//! 算一遍（与 `tests/search_total.rs` 同法）。
//!
//! # 夹具
//!
//! 40 篇、4 个目录、跨轮复用（判据是「`.md` 正好 40 篇」）。形状里刻意留了
//! 三种边角：CRLF 行尾、超过 240 字符的长行（摘要窗口）、以及同一篇里
//! 5 次命中（每篇截断）。

use std::fs;
use std::path::{Path, PathBuf};

use ki_core::SearchHit;
use ki_ipc::search::service::hooks;
use ki_ipc::search::{invalidate, service, sync_after_scan};
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 40;
const DIRS: usize = 4;

/// 三条通道各一条，外加两条全扫（单字在索引里没有 token）。
///
/// `笔` 与 `第` 走 `Mode::FullScan`——它的 pool 是**全库**，与 `indexed_search`
/// 共用同一个并行内核，必须一起测。
const QUERIES: &[&str] = &[
    "样本",         // CJK：命中全库，每篇 5 行
    "笔记 下的第",  // CJK 多词 AND
    "alpha",        // ASCII 词
    "样本 笔记",    // 两个词在同一行
    "笔",           // 单字 → 全扫
    "第",           // 单字 → 全扫
    "找不到的词",   // 零命中
];

/// 线程数覆盖的取值。`0` = 自动（`available_parallelism` 钳到 8）。
const THREADS: &[usize] = &[1, 2, 3, 4, 8, 0];

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-search-parallel")
}

/// 每篇 5 次 `样本`：`hits` 每篇截到 3 条，`lines` 必须是 5。
fn body(dir: &str, n: usize) -> String {
    let mut text = format!(
        concat!(
            "# 笔记 {n:03}\n",
            "\n",
            "这是 {dir} 下的第 {n} 篇样本笔记。\n",
            "样本 第二次出现。\n",
            "样本 第三次出现，附带 alpha 关键字。\n",
            "样本 第四次出现。\n",
            "样本 第五次出现。\n",
            "\n",
            "- 条目一\n",
            "- 条目二\n",
            "\n",
            "笔记 与 下的第 在这一行分开出现。\n",
            "正文最后一行。\n",
        ),
        n = n,
        dir = dir,
    );

    // 每 10 篇里留一篇 CRLF：定位器要剥掉行尾的 `\r`，两种行尾必须给出
    // 相同的行号与列号。
    if n % 10 == 5 {
        text = text.replace('\n', "\r\n");
    }

    // 每 10 篇里留一篇超长行：摘要窗口只保留命中附近的一段，`from`/`to`
    // 是相对窗口的偏移。窗口逻辑在并行下不该有任何变化。
    if n % 10 == 7 {
        text.push_str(&"样".repeat(200));
        text.push_str("样本");
        text.push_str(&"尾".repeat(200));
        text.push('\n');
    }

    text
}

fn manifest() -> Vec<(String, String)> {
    (0..FILES)
        .map(|n| {
            let dir = format!("d{:02}", n % DIRS);
            let rel = format!("{dir}/n{n:03}.md");
            (rel, body(&dir, n))
        })
        .collect()
}

fn count_notes(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".knowisle" {
            continue;
        }
        if entry.path().is_dir() {
            count += count_notes(&entry.path());
        } else if name.to_string_lossy().ends_with(".md") {
            count += 1;
        }
    }
    count
}

fn ensure_fixture(root: &Path) -> Vec<(String, String)> {
    let files = manifest();
    assert_eq!(files.len(), FILES, "夹具口径：正好 40 篇");
    if count_notes(root) == FILES {
        return files;
    }
    if root.exists() {
        fs::remove_dir_all(root).expect("清掉上一轮夹具");
    }
    for (rel, content) in &files {
        let abs = root.join(rel);
        fs::create_dir_all(abs.parent().expect("有父目录")).expect("建目录");
        fs::write(&abs, content).expect("写笔记");
    }
    files
}

/// 独立口径：按空白拆词、逐行 AND。**不经过**定位器，也不经过索引。
fn expected(files: &[(String, String)], query: &str) -> usize {
    let words: Vec<&str> = query.split_whitespace().collect();
    assert!(!words.is_empty());
    files
        .iter()
        .map(|(_, content)| {
            content
                .lines()
                .filter(|line| words.iter().all(|word| line.contains(word)))
                .count()
        })
        .sum()
}

/// 一条命中的可比形态。`SearchHit` 不 derive `PartialEq`，而这里要的是
/// **逐字段**相同，不是「条数与首条相同」。
fn shape(hit: &SearchHit) -> (String, u32, u32, u32, u32, u32, String, bool) {
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

fn shapes(hits: &[SearchHit]) -> Vec<(String, u32, u32, u32, u32, u32, String, bool)> {
    hits.iter().map(shape).collect()
}

#[test]
fn hits_are_identical_at_every_thread_count() {
    let root = fixture_root();
    let files = ensure_fixture(&root);

    invalidate(&root);
    let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(&root, &scan).expect("同步");

    // 覆盖真的接上了吗。没接上的话，下面所有线程数跑的都是同一条串行路径，
    // 用例会「通过」但什么都没证明。
    hooks::set_locate_threads(1);
    assert_eq!(hooks::locate_threads_for_test(FILES), 1);
    hooks::set_locate_threads(8);
    assert_eq!(hooks::locate_threads_for_test(FILES), 8, "8 线程的池必须用 8 个线程");
    assert_eq!(hooks::locate_threads_for_test(1), 1, "单候选必须串行");
    assert_eq!(hooks::locate_threads_for_test(2), 1, "两篇也必须串行");
    assert_eq!(hooks::locate_threads_for_test(3), 3, "三篇开始并行");
    assert_eq!(hooks::locate_threads_for_test(1000), 8, "线程数钳到 8");

    for query in QUERIES {
        hooks::set_locate_threads(1);
        let base = service::search(&root, query, None).expect("搜索");

        // `total` 的独立口径。零命中的查询天然对得上（0 == 0），其余要有货。
        let oracle = expected(&files, query);
        assert_eq!(base.total, oracle, "{query:?} 的 total 必须等于逐行 AND 的行数");
        assert!(!base.truncated, "{query:?} 的候选池没被截断，total 是准确值");

        for &threads in THREADS {
            hooks::set_locate_threads(threads);
            let other = service::search(&root, query, None).expect("搜索");

            assert_eq!(
                other.total, base.total,
                "{query:?} 在 {threads} 线程下 total 变了"
            );
            assert_eq!(
                other.truncated, base.truncated,
                "{query:?} 在 {threads} 线程下 truncated 变了"
            );
            assert_eq!(
                shapes(&other.hits),
                shapes(&base.hits),
                "{query:?} 在 {threads} 线程下 hits 不是逐元素相同"
            );
        }
    }

    // 每篇 5 次命中、`MAX_HITS_PER_FILE = 3`：这条钉住「返回条数」与
    // 「计数」是两件事——并行化最容易顺手改坏的就是它。
    hooks::set_locate_threads(8);
    let capped = service::search(&root, "样本", None).expect("搜索");
    assert_eq!(
        capped.hits.len(),
        FILES * 3,
        "每篇只返回 3 条，40 篇共 120 条"
    );
    assert_eq!(
        capped.total,
        expected(&files, "样本"),
        "但 total 数的是全部命中行，不是返回条数"
    );

    // 读不到的一篇 ⇒ `total` 是下界，且这个信号要**跨线程**汇总。
    // 删掉磁盘上的文件但不重扫：候选池里仍然有它（这正是真实场景——
    // 扫描与搜索之间用户删了一篇）。
    let full_total = expected(&files, "样本");
    let missing = root.join("d01/n005.md");
    let content = files
        .iter()
        .find(|(rel, _)| rel == "d01/n005.md")
        .map(|(_, content)| content.clone())
        .expect("夹具里有这一篇");
    fs::remove_file(&missing).expect("删掉一篇");

    let mut totals = Vec::new();
    for &threads in &[1usize, 2, 4, 8] {
        hooks::set_locate_threads(threads);
        let result = service::search(&root, "样本", None).expect("搜索");
        assert!(
            result.truncated,
            "{threads} 线程：少了那一篇的命中，total 必须降级为下界"
        );
        assert_eq!(
            result.total,
            full_total - 5,
            "{threads} 线程：只该少那一篇的 5 行"
        );
        totals.push(result.total);
    }
    assert_eq!(
        totals,
        vec![full_total - 5; 4],
        "读不到的那篇要在每个线程数下都算进去"
    );

    // 把夹具还原，让下一轮不必重建。
    fs::write(&missing, content).expect("还原这一篇");

    hooks::set_locate_threads(0);
}
