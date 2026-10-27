//! `total` 的**绝对数**契约：1000 篇夹具上的三条查询。
//!
//! # 为什么只能靠绝对数
//!
//! `search_parity.rs` 让新旧两个引擎共用同一个 `NoteLocator`——改定位器两边
//! 一起变，对拍恒绿。所以「`total` 是不是真值」在这一层是**对拍证明不了的**，
//! 只能拿一份形状已知的夹具把答案算出来再比：1000 篇、每篇正文一行
//! `这是 {dir} 下的第 {n} 篇样本笔记。`，于是
//!
//! | 查询 | 全库字面 AND 命中行 | 说明 |
//! |---|---|---|
//! | `样本笔记` | 1000 | 每篇恰好一行 |
//! | `笔记 下的第` | 1000 | `笔记` 由「样本笔记」提供，`下的第` 由正文提供 |
//! | `笔记 下的第 7` | 271 | 再要求行内有 `7`：编号 0000–0999 里含 7 的有 271 个 |
//!
//! # 夹具与 e2e 同形，但不共用文件
//!
//! 形状照抄 `scripts/e2e/fixtures.mjs`（同一批目录/文件名/正文模板），
//! 因为「1000 篇」是那边的验收口径，两边数字必须能对上。但文件不共用：
//! 那是 node 侧的资产，这里是 Rust 侧的契约；共用一份会让两边互相污染
//! （B-32：上一轮的残留改变这一轮的起点）。
//!
//! 夹具落在系统临时目录并**跨轮复用**（1000 篇的索引重建在 debug 下要 8 秒，
//! 每轮重付一次太贵）。判据是「`.md` 数量正好 1000」：上一轮多出或少了
//! 一篇就整块重建，所以复用不会把起点弄歪。

use std::fs;
use std::path::{Path, PathBuf};

use ki_ipc::search::{invalidate, service, sync_after_scan};
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 1000;

fn pad(i: usize) -> String {
    format!("{i:04}")
}

fn body(title: &str, dir: &str, n: usize) -> String {
    format!("# {title}\n\n这是 {dir} 下的第 {n} 篇样本笔记。\n\n- 条目一\n- 条目二\n\n正文最后一行。\n")
}

/// 与 `scripts/e2e/fixtures.mjs` 的 `manifest()` 同一份形状。
fn manifest() -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    let mut batch = |dir: &str, count: usize, make: &dyn Fn(usize) -> String, special: &[&str]| {
        for name in special {
            files.push((dir.to_string(), (*name).to_string()));
        }
        for i in 0..count - special.len() {
            files.push((dir.to_string(), make(i)));
        }
    };
    let even_odd = |even: &'static str, odd: &'static str| {
        move |i: usize| {
            if i % 2 == 0 {
                format!("{even}{}.md", pad(i))
            } else {
                format!("{odd}{}.md", pad(i))
            }
        }
    };

    batch("inbox", 60, &even_odd("收件箱-", "inbox-"), &[]);
    batch("notes", 100, &even_odd("note-", "会议记录-"), &["foobar.md"]);
    batch("notes/sub-a", 60, &even_odd("sub-a-", "项目笔记-"), &[]);
    batch("notes/sub-b", 60, &even_odd("sub-b-", "阅读摘录-"), &[]);
    batch("docs", 80, &even_odd("guide-", "规范-"), &["readme.md"]);
    batch("archive", 80, &even_odd("archive-", "旧稿-"), &[]);
    batch("recipes", 60, &even_odd("recipe-", "食谱-"), &["almond-dessert/menu.md"]);
    batch("projects", 159, &|i| match i % 3 {
        0 => format!("Ki-Store-{}.md", pad(i)),
        1 => format!("DESIGN-{}.md", pad(i)),
        _ => format!("project-{}.md", pad(i)),
    }, &[]);
    batch("笔记", 100, &|i| {
        if i % 2 == 0 {
            format!("第{}篇-读书摘要.md", pad(i))
        } else {
            format!("第{}篇-会议记录.md", pad(i))
        }
    }, &["知识管理.md"]);
    batch("杂项", 60, &even_odd("杂项-", "随想-"), &["认知识别.md"]);
    batch("中文/长篇阅读", 60, &|i| format!("长篇-{}.md", pad(i)), &[]);
    batch("foo", 40, &|i| format!("foo-{}.md", pad(i)), &["bar.md"]);
    batch("alpha", 40, &|i| format!("Alpha-{}.md", pad(i)), &["API-Design.md"]);
    batch("beta", 40, &|i| format!("beta-{}.md", pad(i)), &["api-design.md"]);
    batch("bar", 1, &|_| "unused.md".to_string(), &["notes.md"]);

    files
        .into_iter()
        .enumerate()
        .map(|(index, (dir, name))| {
            let title = name.trim_end_matches(".md");
            let rel = format!("{dir}/{name}");
            (rel, body(title, &dir, index))
        })
        .collect()
}

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-search-total")
}

/// 数库里有多少篇笔记（跳过 `.knowisle/`——索引库不是笔记）。
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
    assert_eq!(files.len(), FILES, "夹具口径：正好 1000 篇");
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

#[test]
fn three_queries_report_exact_totals() {
    let root = fixture_root();
    let files = ensure_fixture(&root);

    invalidate(&root);
    let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(&root, &scan).expect("同步");

    // 每一行都钉死「夹具没变」+「total 是真值」两件事。硬编码的期望值是
    // 夹具的口径，`expected()` 是独立的算法；两者不等说明夹具被改过。
    for (query, want) in [
        ("样本笔记", 1000usize),
        ("笔记 下的第", 1000),
        ("笔记 下的第 7", 271),
    ] {
        let oracle = expected(&files, query);
        assert_eq!(oracle, want, "{query:?} 的夹具口径变了");
        let result = service::search(&root, query, None).expect("搜索");
        assert_eq!(
            result.total, oracle,
            "{query:?} 的 total 必须等于全库字面 AND 命中行数"
        );
        assert!(
            !result.truncated,
            "{query:?} 的 total 是准确值，不许标成下界"
        );
        assert_eq!(result.hits.len(), 200, "{query:?} 仍然只返回前 200 条");
    }
}
