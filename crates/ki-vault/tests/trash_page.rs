//! 回收站分页：10k 夹具、首屏计时、以及端到端验收的前置夹具。
//!
//! # 为什么夹具不用 `delete_note` 造
//!
//! 一万次删除 = 一万次 `rename` + 一万次 `tmp → fsync → rename` 写清单，
//! 单是 fsync 就要几十秒。分页要验的是**读侧**的成本结构，夹具直接摆出
//! 「回收目录里有一万个文件 + 一万条清单」这个终态就够了：`File::create`
//! 造 0 字节条目，清单直接写 JSON。
//!
//! # 为什么计时必须是 release
//!
//! B-27 的教训：ADR-007 里「10k 篇约 5 秒」是拍脑袋写的，release 实测是
//! 16–108 秒。debug 的 `serde_json` 与 `read_dir` 慢 5–20 倍，拿它估出来的
//! 比例没有任何意义。本文件的计时用例默认 `#[ignore]`，只在 release 下显式跑：
//!
//! ```sh
//! cargo test -p ki-vault --release --test trash_page bench_ -- --ignored --nocapture
//! ```

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ki_vault::{trash_page, MANIFEST_DIR, TRASH_DIR};

/// 夹具的删除时间起点（2023-11-14T22:13:20Z）。每条 +1ms，于是排序是确定的。
const BASE_STAMP: i64 = 1_700_000_000_000;

/// 造 `count` 条回收站条目：0 字节文件 + 一条对应的清单记录。
///
/// 名字用 `<stamp>-n00000.md`，与 `delete_note` 产出的形状一致
/// （`fileops::delete_note` 用同一个 `stamp` 写文件名与清单 `deletedAt`）。
fn build_fixture(root: &Path, count: usize) {
    let trash = root.join(TRASH_DIR);
    let manifest = root.join(MANIFEST_DIR);
    fs::create_dir_all(&trash).unwrap();
    fs::create_dir_all(&manifest).unwrap();

    for index in 0..count {
        let stamp = BASE_STAMP + index as i64;
        let entry = format!("{stamp}-n{index:05}.md");
        fs::File::create(trash.join(&entry)).unwrap();
        fs::write(
            manifest.join(format!("{entry}.json")),
            format!(r#"{{"origin":"notes/n{index:05}.md","deletedAt":{stamp}}}"#),
        )
        .unwrap();
    }
}

fn report(label: &str, samples: &[Duration]) {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    let min = sorted[0].as_secs_f64() * 1000.0;
    let median = sorted[sorted.len() / 2].as_secs_f64() * 1000.0;
    let max = sorted[sorted.len() - 1].as_secs_f64() * 1000.0;
    println!(
        "{label}: min {min:.2}ms / median {median:.2}ms / max {max:.2}ms（{} 次）",
        samples.len()
    );
}

/// 验收 1 + 2：10k 条上的首屏与「一路翻到底不重不漏」。
///
/// 判据全部是**集合**而不是计数：`seen.len() == 10_000` 只能证明没有重复，
/// 与磁盘上的文件集合相等才能证明没有漏。
#[test]
#[ignore = "10k 条夹具要几十秒，显式跑：cargo test -p ki-vault --release --test trash_page -- --ignored --nocapture"]
fn ten_thousand_entries_page_through_without_gaps() {
    let vault = tempfile::TempDir::new().unwrap();
    build_fixture(vault.path(), 10_000);

    let start = Instant::now();
    let first = trash_page(vault.path(), None, 200).unwrap();
    let first_page = start.elapsed();
    assert_eq!(first.entries.len(), 200, "首屏正好一页");
    assert_eq!(first.total, 10_000, "total 是全量，不是这一页");
    assert!(first.next.is_some(), "还有 9800 条，必须有下一页");

    let mut seen: HashSet<String> = first.entries.iter().map(|e| e.entry.clone()).collect();
    let mut cursor = first.next.clone();
    let mut pages = 1usize;
    while let Some(position) = cursor {
        let page = trash_page(vault.path(), Some(&position), 200).unwrap();
        assert_eq!(page.total, 10_000, "total 不随翻页变化");
        for entry in &page.entries {
            assert!(
                seen.insert(entry.entry.clone()),
                "翻页重复返回了 {}",
                entry.entry
            );
        }
        cursor = page.next;
        pages += 1;
    }

    assert_eq!(seen.len(), 10_000, "翻页总数不对：漏了或重了");
    let on_disk: HashSet<String> = fs::read_dir(vault.path().join(TRASH_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        seen, on_disk,
        "翻页拿到的集合必须与 read_dir 的文件集合相等"
    );

    println!(
        "page(null, 200) 首屏 {:.2}ms，共 {pages} 页",
        first_page.as_secs_f64() * 1000.0
    );
}

/// 首屏耗时：10k 条回收站，`page(root, null, 200)` 只读一页的清单。
///
/// 这是 ADR-008 分页小节里「新实现」那个数字的来源，与旧实现的全量
/// `list` 同夹具、同机器、同 release 配置。跑法见文件头。
#[test]
#[ignore = "release 下显式跑：cargo test -p ki-vault --release --test trash_page bench_ -- --ignored --nocapture"]
fn bench_first_page_on_10k() {
    let vault = tempfile::TempDir::new().unwrap();
    build_fixture(vault.path(), 10_000);

    // 预热一次：把目录项与清单页缓存拉进来。冷启动的数字由 B-27 单独记，
    // 这里要的是「用户第二次打开面板」的成本结构，两者不能混。
    let warm = trash_page(vault.path(), None, 200).unwrap();
    assert_eq!(warm.total, 10_000);

    let mut samples = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let page = trash_page(vault.path(), None, 200).unwrap();
        samples.push(start.elapsed());
        assert_eq!(page.entries.len(), 200, "一页 200 条");
        assert_eq!(page.total, 10_000);
    }
    report("page(null, 200) 首屏", &samples);
}

/// 端到端验收用的夹具：`KI_E2E_FIXTURE` 指向一个**系统临时目录**下的目录。
///
/// # 为什么清理放在 Rust 侧
///
/// 本机 node 把 `rmSync` 转发给「安全删除」shim：单次调用超过 50 个目标就
/// `BULK_CONFIRM_REQUIRED`（TOOLING）。两万个文件清不掉，夹具就不可能幂等。
/// Rust 的 `remove_dir_all` 没有这个限制，也不受 Windows 长路径影响。
///
/// # 为什么必须在系统临时目录，不能放仓库的 `.tmp`
///
/// 本机实测：同一个盘上，`%TEMP%` 里删一个文件 1.8ms，仓库的 `.tmp` 里
/// **390ms**（100 个文件 39 秒）。两万个文件差 2 小时——清空那一步在
/// `.tmp` 里根本跑不完，而它正是验收 7。判据因此从「路径含 `.tmp`」改成
/// 「在系统临时目录下」：比原来更严（不会指向真人的库），也跑得动。
#[test]
#[ignore = "由 .tmp/cdp-trash-page.mjs 调用"]
fn prepare_e2e_fixture() {
    let Ok(dir) = std::env::var("KI_E2E_FIXTURE") else {
        eprintln!("没有 KI_E2E_FIXTURE，跳过");
        return;
    };
    let root = PathBuf::from(dir);
    // 兜底：这个用例会 `remove_dir_all`，绝不允许被指向真人的库。
    let temp = std::env::temp_dir();
    assert!(
        root.starts_with(&temp),
        "夹具路径必须在系统临时目录（{}）下：{}",
        temp.display(),
        root.display()
    );

    if root.exists() {
        fs::remove_dir_all(&root).expect("清掉上一轮的夹具");
    }
    fs::create_dir_all(&root).unwrap();
    // 树要能渲染出来：端到端要先看到 `.tree-row` 才继续。
    fs::write(root.join("hello.md"), "# hello\n\n端到端夹具。\n").unwrap();

    build_fixture(&root, 10_000);
    println!("fixture ready: {}", root.display());
}
