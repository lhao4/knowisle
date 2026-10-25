//! 增量同步的验收：第二次同步必须读 **0 字节正文**。
//!
//! # 为什么这条断言是整个存储层最重要的一条
//!
//! "增量"这两个字如果只是注释，那它随时会退化——加一个字段、改一个判据，
//! 就可能变成"每次扫描都全库重读"，而那种退化**没有任何报错**，
//! 只表现为"扫描越来越慢"。
//!
//! `SyncStats::bytes_read` 是唯一的硬证据：它是真正从磁盘读到的正文字节数。
//! 第二次同步只要不是 0，就说明判据失效了。
//!
//! # 为什么这些用例共用一个进程级缓存也不会互相干扰
//!
//! `ki_ipc::search` 的缓存是进程全局的，键是 Vault 根。每个用例用自己的
//! 临时目录，切换时 `ensure_root` 会释放上一个 Vault 的连接。索引本身在磁盘上，
//! 所以"换了个 Vault 再换回来"只是多一次 `IndexDb::open`，不影响正确性。

use std::fs;
use std::path::Path;

use ki_ipc::search::sync_after_scan;
use ki_store::{IndexDb, SyncStats};
use ki_vault::{scan_vault, ScanOptions, MAX_INDEX_BYTES};

fn vault() -> tempfile::TempDir {
    tempfile::tempdir().expect("创建临时目录")
}

/// 扫一遍 + 同步，返回统计。
fn sync(root: &Path) -> SyncStats {
    let scan = scan_vault(root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(root, &scan).expect("同步").stats
}

/// 打开一个独立连接读索引，用来验证"存进去的到底是什么"。
fn inspect(root: &Path) -> IndexDb {
    IndexDb::open(root).expect("打开索引库").0
}

#[test]
fn first_sync_indexes_everything_and_second_reads_nothing() {
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha 知识库").expect("写 a");
    fs::write(dir.path().join("b.md"), "bravo 索引").expect("写 b");

    let first = sync(dir.path());
    assert_eq!(first.inserted, 2);
    assert!(first.bytes_read > 0, "第一次必须真的读正文");
    assert_eq!(first.updated + first.deleted + first.unchanged, 0);

    // 判据本体。
    let second = sync(dir.path());
    assert_eq!(second.bytes_read, 0, "第二次同步不许再读正文");
    assert_eq!(second.unchanged, 2);
    assert_eq!(second.inserted + second.updated + second.deleted, 0);
}

#[test]
fn new_file_is_picked_up_by_the_next_scan() {
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha").expect("写 a");
    sync(dir.path());

    fs::write(dir.path().join("b.md"), "bravo").expect("写 b");
    let stats = sync(dir.path());
    assert_eq!(stats.inserted, 1);
    assert_eq!(stats.unchanged, 1);

    assert_eq!(inspect(dir.path()).counts().expect("读计数").ok, 2);
}

#[test]
fn deleted_file_disappears_from_the_index() {
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha").expect("写 a");
    fs::write(dir.path().join("b.md"), "bravo").expect("写 b");
    sync(dir.path());

    fs::remove_file(dir.path().join("b.md")).expect("删 b");
    let stats = sync(dir.path());
    assert_eq!(stats.deleted, 1);
    assert_eq!(stats.unchanged, 1);

    let db = inspect(dir.path());
    assert_eq!(db.counts().expect("读计数").ok, 1);
    assert!(db.match_ascii("\"bravo\"", 10).expect("查").is_empty());
}

#[test]
fn changed_content_is_reindexed_and_old_tokens_are_gone() {
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha 知识库").expect("写 a");
    sync(dir.path());

    fs::write(dir.path().join("a.md"), "charlie 岛屿检索").expect("改写 a");
    let stats = sync(dir.path());
    assert_eq!(stats.updated, 1);
    assert!(stats.bytes_read > 0);

    let db = inspect(dir.path());
    assert!(
        db.match_ascii("\"alpha\"", 10).expect("查旧词").is_empty(),
        "旧 token 必须被删掉，否则会留下幽灵命中"
    );
    assert_eq!(db.match_ascii("\"charlie\"", 10).expect("查新词").len(), 1);
    // 通道 B：中文 bigram 也必须更新
    assert_eq!(
        db.match_zh("\"岛屿\" AND \"检索\"", 10)
            .expect("查中文")
            .len(),
        1
    );
    assert!(db.match_zh("\"知识\"", 10).expect("查旧中文").is_empty());
}

/// 扫描被截断时**绝不删**：缺失的路径只代表"没扫到"，不代表"文件没了"。
///
/// 反例的后果很具体：20k 上限之后的所有笔记会被一次性从索引里抹掉，
/// 而用户只会看到"搜不到一半的笔记"。
///
/// 这条用例做成对照实验：同一份磁盘状态下，截断的扫描不删、正常的扫描删。
/// 只断言"截断时不删"是不够的——那可能只是因为根本没扫到要删的东西。
#[test]
fn truncated_scan_never_deletes() {
    let dir = vault();
    fs::write(dir.path().join("a.md"), "alpha").expect("写 a");
    fs::write(dir.path().join("b.md"), "bravo").expect("写 b");
    sync(dir.path());
    assert_eq!(inspect(dir.path()).counts().expect("读计数").ok, 2);

    // 扫描（此时两个都在）之后把结果标成截断，再从磁盘删掉 b
    let mut stale = scan_vault(dir.path(), &ScanOptions::default()).expect("扫描");
    stale.truncated = true;
    fs::remove_file(dir.path().join("b.md")).expect("删 b");

    let stats = sync_after_scan(dir.path(), &stale).expect("同步").stats;
    assert_eq!(stats.deleted, 0, "截断的扫描结果不许用来删行");
    assert_eq!(stats.unchanged, 2);
    assert_eq!(
        inspect(dir.path()).counts().expect("读计数").ok,
        2,
        "截断期间 b.md 必须留在索引里"
    );

    // 对照：不截断的正常扫描会把它删掉，证明上面那条断言不是碰巧成立
    let stats = sync(dir.path());
    assert_eq!(stats.deleted, 1);
    assert_eq!(inspect(dir.path()).counts().expect("读计数").ok, 1);
}

/// 超大文件不读正文，且**不重试**：下次扫描看到同样的 size/mtime 就跳过。
#[test]
fn oversized_files_are_skipped_without_reading_and_not_retried() {
    let dir = vault();
    let big = vec![b'a'; (MAX_INDEX_BYTES + 1) as usize];
    fs::write(dir.path().join("big.md"), &big).expect("写超大文件");

    let first = sync(dir.path());
    assert_eq!(first.skipped, 1);
    assert_eq!(first.bytes_read, 0, "超大文件必须在读之前就被挡掉");

    let second = sync(dir.path());
    assert_eq!(second.skipped, 0);
    assert_eq!(second.unchanged, 1);
    assert_eq!(second.bytes_read, 0);

    let counts = inspect(dir.path()).counts().expect("读计数");
    assert_eq!((counts.ok, counts.skipped), (0, 1));
}

/// 非 UTF-8 文件只跳过并计数，不让整次同步失败。
#[test]
fn invalid_utf8_file_is_skipped_not_fatal() {
    let dir = vault();
    fs::write(dir.path().join("gbk.md"), [0xB0, 0xA1, 0xB0, 0xA1]).expect("写非 UTF-8");
    fs::write(dir.path().join("ok.md"), "alpha").expect("写正常文件");

    let stats = sync(dir.path());
    assert_eq!(stats.inserted, 1);
    assert_eq!(stats.skipped, 1);

    let counts = inspect(dir.path()).counts().expect("读计数");
    assert_eq!((counts.ok, counts.skipped), (1, 1));
}

/// 两个 Vault 不能串味：切库时缓存的连接必须被释放。
#[test]
fn two_vaults_do_not_share_an_index() {
    let first = vault();
    let second = vault();
    fs::write(first.path().join("only-first.md"), "alpha").expect("写");
    fs::write(second.path().join("only-second.md"), "bravo").expect("写");

    sync(first.path());
    sync(second.path());

    let db = inspect(first.path());
    assert_eq!(db.counts().expect("读计数").ok, 1);
    assert_eq!(db.match_ascii("\"alpha\"", 10).expect("查").len(), 1);
    assert!(
        db.match_ascii("\"bravo\"", 10).expect("查").is_empty(),
        "第一个 Vault 的索引里不该有第二个 Vault 的内容"
    );
}

/// 空库也要能同步：不该报错，也不该写入任何东西。
#[test]
fn empty_vault_syncs_cleanly() {
    let dir = vault();
    let stats = sync(dir.path());
    assert_eq!(stats.bytes_read, 0);
    assert_eq!(
        stats.inserted + stats.updated + stats.deleted + stats.unchanged,
        0
    );
    assert_eq!(inspect(dir.path()).counts().expect("读计数").ok, 0);
}
