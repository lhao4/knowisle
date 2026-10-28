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

/// 认不出编码的文件只跳过并计数，不让整次同步失败。
#[test]
fn undecodable_file_is_skipped_not_fatal() {
    let dir = vault();
    // `01 FF 02` 解出来只可能是控制字符：四道探测（BOM / NUL 模式 /
    // 严格 UTF-8 / 猜）全过不去。
    // （M7 第八刀之前这里放的是 GBK 字节 `B0 A1 B0 A1`——它现在解得开，
    // 不再是「坏文件」，见下面那条用例。）
    fs::write(dir.path().join("bad.md"), [0x01, 0xFF, 0x02]).expect("写认不出编码的文件");
    fs::write(dir.path().join("ok.md"), "alpha").expect("写正常文件");

    let stats = sync(dir.path());
    assert_eq!(stats.inserted, 1);
    assert_eq!(stats.skipped, 1);

    let counts = inspect(dir.path()).counts().expect("读计数");
    assert_eq!((counts.ok, counts.skipped), (1, 1));
}

/// GBK 笔记进得了索引，也搜得到——「看得见搜不到」那一半在这一刀被修掉。
///
/// 判据是 `total` 与 `hits` 都要对，且**两种编码各一篇对照**：只放一篇 GBK
/// 的话，「搜到了」可能只是因为文件名撞上了查询词。
#[test]
fn gbk_note_is_indexed_and_searchable() {
    let dir = vault();
    // 「中文笔记」的 GBK 字节。
    fs::write(
        dir.path().join("gbk.md"),
        [0xD6u8, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x0A],
    )
    .expect("写 GBK 文件");
    fs::write(dir.path().join("utf8.md"), "对照：中文笔记\n").expect("写 UTF-8 对照");

    let stats = sync(dir.path());
    assert_eq!(stats.inserted, 2, "两篇都该进索引");
    assert_eq!(stats.skipped, 0, "GBK 不再是「跳过」");

    let counts = inspect(dir.path()).counts().expect("读计数");
    assert_eq!((counts.ok, counts.skipped), (2, 0));

    let hits = ki_ipc::search::service::search(dir.path(), "中文笔记", None).expect("搜索");
    assert_eq!(hits.total, 2, "GBK 那篇的正文子串必须命中");
    let paths: Vec<&str> = hits.hits.iter().map(|hit| hit.path.as_str()).collect();
    assert!(paths.contains(&"gbk.md"), "实际命中：{paths:?}");
    assert!(paths.contains(&"utf8.md"), "实际命中：{paths:?}");
}

/// 版本不符 → 整库重建 → 上一篇被跳过的笔记复活。
///
/// 这是「被跳过的篇不会自己复活」那条病的解药：跳过记录用扫描时的
/// size/mtime 落库，文件没动就永远判「未变」。只有 `TOKENIZER_VERSION`
/// 变了才会整库重来，那篇才会被重新读一次。
///
/// 用例把 `meta.tokenizer_version` 改回上一版来模拟「旧库」——不能靠
/// 编译期常量，那个值在本进程里是固定的。
#[test]
fn a_skipped_note_revives_after_a_version_bump() {
    let dir = vault();
    let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x0A];
    fs::write(dir.path().join("gbk.md"), gbk).expect("写 GBK 文件");
    sync(dir.path());

    // 手工把库改回「旧版本 + 这篇是跳过的」。
    //
    // 注意不能用 `inspect()` 验前置状态：`IndexDb::open` 一看到版本不符就会
    // **当场整库重建**，把刚做出来的旧库抹掉。所以下面用裸连接读。
    {
        let conn = rusqlite::Connection::open(dir.path().join(".knowisle/index.sqlite"))
            .expect("打开索引库");
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'tokenizer_version'",
            ["bigram-v2/ascii-proj-v2"],
        )
        .expect("改版本");
        conn.execute(
            "UPDATE note SET status = 'skipped', skip_reason = 'invalid_utf8' WHERE path = 'gbk.md'",
            [],
        )
        .expect("改成跳过");

        let skipped: i64 = conn
            .query_row(
                "SELECT count(*) FROM note WHERE status = 'skipped'",
                [],
                |r| r.get(0),
            )
            .expect("读计数");
        assert_eq!(skipped, 1, "前置状态：这篇在旧库里是跳过的");
    }

    // 再同步一次：版本不符 → 整库重建 → 全量重读。
    let stats = sync(dir.path());
    assert_eq!(stats.inserted, 1, "重建之后这篇是「新插入」");
    assert_eq!(
        inspect(dir.path()).counts().expect("读计数"),
        ki_store::Counts { ok: 1, skipped: 0 },
        "重建之后它必须是 ok"
    );

    let hits = ki_ipc::search::service::search(dir.path(), "中文笔记", None).expect("搜索");
    assert_eq!(hits.total, 1);
    assert_eq!(hits.hits[0].path, "gbk.md");
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
