//! FTS5 分词器的两条**能力边界**，用可执行判据钉住。
//!
//! 这两条用例存在的理由不是「我们用了 trigram」——我们**没有**用它。它们记录
//! 的是「为什么没选它」，而那个理由是下次有人提议 trigram 时最需要的证据：
//!
//! 1. **trigram 有 3 字符下限。** 官方文档（FTS5 4.3.4 The Trigram Tokenizer）：
//!    "Substrings consisting of fewer than 3 unicode characters do not match any
//!    rows when used with a full-text query." 而当初那个例子恰好是 2 字符的
//!    `us`——trigram 通道修不了它。**文档这么写不算数**，这里跑一遍。
//! 2. **trigram 与 `contentless_delete=1` 是兼容的。** 这个组合没有先例，
//!    当时不知道能不能建；实测能建、能删、删完 MATCH 归零。所以拒绝 trigram
//!    的理由只剩体积（10k 夹具 +34.3%），不是「建不出来」。
//!
//! 数字与结论见 `docs/ARCHITECTURE.md`「ASCII 词中子串」那一节。

use rusqlite::Connection;

/// `MATCH` 命中的行数。表达式由测试自己拼，不经过 `ki-search`。
fn hits(conn: &Connection, table: &str, query: &str) -> i64 {
    conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE {table} MATCH ?1"),
        [query],
        |r| r.get(0),
    )
    .unwrap_or_else(|e| panic!("{table} MATCH {query:?} 失败：{e}"))
}

/// 两字符的查询在 trigram 表上一行都匹配不到；三字符起才行。
///
/// 这条是「trigram 修不了 `us`」的全部证据。它不依赖我们的 schema，
/// 只依赖 SQLite 的行为——所以它同时也是一条**版本哨兵**：哪天 bundled 的
/// SQLite 改了这条规则（比如支持 2 字符），这里会红。
#[test]
fn trigram_does_not_match_queries_shorter_than_three_chars() {
    let conn = Connection::open_in_memory().expect("内存库");
    conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(a, tokenize='trigram')")
        .expect("建 trigram 表");
    conn.execute("INSERT INTO t(rowid, a) VALUES(1, 'rust')", [])
        .expect("写一行");

    // 1–2 字符：全部 0 行。`us` 就是那个经典例子。
    for query in ["u", "us", "ow", "ru", "st"] {
        assert_eq!(hits(&conn, "t", query), 0, "{query:?} 不该命中");
    }

    // ≥3 字符：子串语义成立。
    for query in ["rus", "ust", "rust"] {
        assert_eq!(hits(&conn, "t", query), 1, "{query:?} 应当命中");
    }
}

/// trigram 与 `contentless_delete=1` 能共存：建表、写入、MATCH、删除都正常。
///
/// 参数与 `note_fts_zh` 同形，只把 `tokenize` 换成 `trigram`——这正是当初
/// 「没人试过」的那个组合。
#[test]
fn trigram_works_with_contentless_delete() {
    let conn = Connection::open_in_memory().expect("内存库");
    conn.execute_batch(
        "CREATE VIRTUAL TABLE t USING fts5(
           body_ascii,
           content='',
           contentless_delete=1,
           tokenize='trigram'
         )",
    )
    .expect("trigram + contentless_delete 必须能建表");

    conn.execute("INSERT INTO t(rowid, body_ascii) VALUES(1, 'rust')", [])
        .expect("写入");
    assert_eq!(hits(&conn, "t", "rus"), 1);

    conn.execute("DELETE FROM t WHERE rowid = 1", [])
        .expect("contentless_delete 必须支持 DELETE");
    assert_eq!(hits(&conn, "t", "rus"), 0, "删掉之后不该还能命中");
}
