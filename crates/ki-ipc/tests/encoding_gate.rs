//! 门禁③：加一列的三笔成本。
//!
//! ```sh
//! cargo test -p ki-ipc --release --test encoding_gate -- --ignored --nocapture
//! ```
//!
//! # 三笔账
//!
//! | 记号 | 量什么 | 门禁 |
//! |---|---|---|
//! | a | 加列 + 部分索引之后 `index.sqlite` 的字节增量 | ≤ +2% |
//! | b | 建库耗时增量 | ≤ +3% |
//! | c | 1000 篇 GBK 批量转存的墙钟与失败率 | ≤ 90s / 失败率 0 |
//!
//! # a 的测法：同一份库，把新增对象拆掉再比
//!
//! 造两份 10k 库再比较是错的——两次建库的页填充率、WAL 检查点时机都不同，
//! 噪声比这一列本身还大。这里只建**一份**，`VACUUM` 之后量一次，再
//! `DROP INDEX` + `ALTER TABLE ... DROP COLUMN` + `VACUUM` 量一次。
//! 同一份数据、同一个文件，差的就是新增对象占的字节。
//!
//! # b 的测法
//!
//! 单进程内交替建两次（同一份夹具、同一份代码），取第二次的口径：
//! 第一次要把 10k 篇读进页缓存，第二次才是「稳定态」的建库成本。
//! 增量由 `--ignored` 的两轮（改前 / 改后各跑一次）比较得出，
//! **同一台机、同一天**才有意义。

mod common;

use std::fs;
use std::path::Path;
use std::time::Instant;

use ki_ipc::search::sync_after_scan;
use ki_ipc::transcode;
use ki_vault::encoding::{encode, Encoding};
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 10_000;
const DIRS: usize = 50;

fn db_path(root: &Path) -> std::path::PathBuf {
    root.join(ki_core::METADATA_DIR).join("index.sqlite")
}

/// 删掉索引库（连 `-wal` / `-shm`），让下一次 `sync_after_scan` 从零建。
///
/// **必须先 `invalidate`**：进程级缓存还握着上一次的连接，Windows 上
/// 「删除一个被打开的文件」会失败——而 `remove_file` 的错误被忽略之后，
/// 下一次同步看到的是「10k 篇全都没变」，`inserted` 是 0，看起来像
/// 「建库什么都没写」。这个坑正是 `tests/sync.rs` 里那条 `bytes_read == 0`
/// 的反面。
fn reset_index(root: &Path) {
    ki_ipc::search::invalidate(root);
    for suffix in ["", "-wal", "-shm"] {
        fs::remove_file(format!("{}{suffix}", db_path(root).display()))
            .unwrap_or_else(|err| assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "删索引库"));
    }
}

fn build_once(root: &Path) -> (u128, u64) {
    let scan = scan_vault(root, &ScanOptions::default()).expect("扫描");
    let started = Instant::now();
    let outcome = sync_after_scan(root, &scan).expect("同步");
    let elapsed = started.elapsed().as_millis();
    assert!(
        outcome.stats.inserted + outcome.stats.updated > 0,
        "这一轮什么都没写，不是一次建库"
    );
    (elapsed, fs::metadata(db_path(root)).expect("索引库").len())
}

#[test]
#[ignore = "10k 夹具，显式跑"]
fn gate3_index_size_and_build_cost() {
    let root = std::env::temp_dir().join("knowisle-single-char");
    common::ensure_fixture(&root, FILES, DIRS);

    // 第一次：冷缓存（读 10k 篇进页缓存）。报出来，但不作为 b 的口径。
    reset_index(&root);
    let (cold_ms, _) = build_once(&root);
    // 第二次：稳定态。
    reset_index(&root);
    let (warm_ms, _) = build_once(&root);

    // VACUUM 之后才是「这个库真实占多少」——建库过程中的空闲页会被回收。
    //
    // 这里用裸 `rusqlite` 而不是 `IndexDb`：`IndexDb::open` 会体检，而
    // 「把新增对象拆掉」之后的行数/版本就不再是应用认可的状态了。
    // 量测工具不该为了量一次去改生产代码。
    {
        let conn = rusqlite::Connection::open(db_path(&root)).expect("打开索引库");
        conn.execute_batch("VACUUM").expect("vacuum");
    }
    let size_with = fs::metadata(db_path(&root)).expect("索引库").len();

    // 拆掉新增对象，同一份数据再量一次。
    {
        let conn = rusqlite::Connection::open(db_path(&root)).expect("打开索引库");
        conn.execute_batch(
            "DROP INDEX IF EXISTS note_src_encoding;
             ALTER TABLE note DROP COLUMN src_encoding;
             VACUUM;",
        )
        .expect("拆掉新增对象");
    }
    let size_without = fs::metadata(db_path(&root)).expect("索引库").len();

    let delta = size_with as f64 / size_without as f64 - 1.0;
    println!("a) 索引库体积：{size_without} → {size_with} 字节（{:+.3}%）", delta * 100.0);
    println!("b) 建库耗时：冷 {cold_ms}ms / 稳定态 {warm_ms}ms（10k 篇）");

    assert!(
        delta <= 0.02,
        "索引库体积涨了 {:.3}%，门禁是 +2%",
        delta * 100.0
    );
}

/// b 的增量：**写这一列本身**值多少。
///
/// 上面那条量的是「10k 全 UTF-8 的库建一次要多久」，而那种库里这一列全是
/// NULL——它的增量按构造就是 0，量出来只会是噪声。真正要回答的是「如果
/// 每一篇都不是 UTF-8，写这一列要多花多少」：那是上界，上界过了门禁，
/// 常见情形（几十篇）自然过。
///
/// 走的是 `IndexDb::batch` + `upsert_ok`，与索引器同一条路径（同一份 SQL、
/// 同一个事务粒度），不是另写一个微型基准。
#[test]
#[ignore = "10k 次写库，显式跑"]
fn gate3_column_write_cost() {
    let dir = tempfile::tempdir().expect("临时目录");
    let rows: Vec<(String, String)> = (0..10_000)
        .map(|index| {
            let path = format!("d{:02}/n{index:05}.md", index % 50);
            (path, "知识库 索引 检索 笔记".to_string())
        })
        .collect();

    let run = |encoding: Option<&str>| -> u128 {
        let root = dir.path().join(match encoding {
            Some(_) => "with",
            None => "without",
        });
        fs::create_dir_all(&root).expect("建目录");
        let (mut db, _) = ki_store::IndexDb::open(&root).expect("开库");
        let started = Instant::now();
        for chunk in rows.chunks(256) {
            db.batch(|batch| {
                for (path, body) in chunk {
                    batch.upsert_ok(
                        &ki_store::IndexedNote {
                            path: path.clone(),
                            name_lower: path.to_lowercase(),
                            size: body.len() as u64,
                            mtime: 1_700_000_000,
                            body_ascii: String::new(),
                            body_bigram: String::new(),
                            body_tokens: body.clone(),
                            src_encoding: encoding.map(str::to_owned),
                        },
                        1_700_000_000,
                    )?;
                }
                Ok(())
            })
            .expect("写一批");
        }
        started.elapsed().as_millis()
    };

    // 两轮取第二轮：第一轮要建库、热页缓存。
    let _ = run(None);
    let _ = run(Some("GBK"));
    let without = run(None);
    let with = run(Some("GBK"));

    let delta = with as f64 - without as f64;
    println!(
        "b') 写这一列：10k 行 {without}ms → {with}ms（{:+.0}ms，{:+.2}%）",
        delta,
        delta / without as f64 * 100.0
    );
    // 门禁是「建库耗时增量 ≤ +3%」。写库只占建库的一部分，所以这里
    // 直接拿写库自己的相对增量做判据更严：它过不了，整库更过不了。
    assert!(
        delta / without as f64 <= 0.03,
        "写这一列让写库慢了 {:.2}%，门禁 +3%",
        delta / without as f64 * 100.0
    );
}

#[test]
#[ignore = "1000 篇 GBK 真实写盘，显式跑"]
fn gate3_batch_transcode_cost() {
    let root = std::env::temp_dir().join("knowisle-encoding-batch");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("建夹具目录");

    // 1000 篇：正文取真实语料窗口，**只取 GBK 装得下的那些**。
    // 语料里混着 emoji（`❌`），而 GBK 编不出来——夹具造不出来就谈不上量它。
    let windows: Vec<String> = common::corpus_windows()
        .into_iter()
        .filter(|window| encode(window, Encoding::Gbk).is_ok())
        .collect();
    assert!(windows.len() >= 50, "GBK 装得下的窗口太少：{}", windows.len());
    let mut paths = Vec::with_capacity(1000);
    for index in 0..1000 {
        let text = format!(
            "# 待转存 {index:04}\n\n{}\n\n{}\n",
            windows[index % windows.len()],
            windows[(index * 7 + 3) % windows.len()]
        );
        let bytes = encode(&text, Encoding::Gbk).expect("编成 GBK");
        let rel = format!("d{:02}/n{index:04}.md", index % 20);
        fs::create_dir_all(root.join(format!("d{:02}", index % 20))).expect("建子目录");
        fs::write(root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)), &bytes).expect("写");
        paths.push(rel);
    }

    // 先把索引建起来（批量转存之后要靠它把 src_encoding 归零）。
    let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
    let outcome = sync_after_scan(&root, &scan).expect("同步");
    assert_eq!(outcome.stats.inserted, 1000, "索引没建全，后面的口径不准");

    let started = Instant::now();
    let result = transcode::run_batch(&root, paths.clone(), 0xE11);
    let elapsed = started.elapsed().as_secs_f64();

    let rate = result.failed.len() as f64 / paths.len() as f64;
    println!(
        "c) 1000 篇批量转存：{elapsed:.2}s，成功 {} / 失败 {}（失败率 {:.2}%）",
        result.ok.len(),
        result.failed.len(),
        rate * 100.0
    );
    for failure in &result.failed {
        println!("   - {} => {}", failure.path, failure.error);
    }

    // 幂等：再跑一遍，0 篇需要转、`history/` 不新增。
    let history = root.join(ki_core::METADATA_DIR).join("history");
    let before = count_dirs(&history);
    let again = transcode::run_batch(&root, paths, 0xE12);
    let after = count_dirs(&history);
    println!("   幂等复跑：ok={} failed={} history {before} → {after}", again.ok.len(), again.failed.len());

    assert_eq!(result.failed.len(), 0, "批量转存有失败");
    assert!(elapsed <= 90.0, "1000 篇批量转存 {elapsed:.2}s 超过 90s 门禁");
    assert_eq!(before, after, "幂等复跑新增了 history 目录");
}

fn count_dirs(dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            count += 1 + count_dirs(&entry.path());
        }
    }
    count
}
