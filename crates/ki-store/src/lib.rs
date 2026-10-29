//! 知屿 KnowIsle · 索引库（派生数据）
//!
//! 这个 crate 是公理 A1 的物理体现：库里的一切都能从磁盘上的 Markdown 重新
//! 算出来。删掉 `.knowisle/index.sqlite`，搜索功能仍然完整，只是第一次搜索要
//! 重建索引。
//!
//! # 分层
//!
//! - [`schema`]：版本号、DDL、`meta` 键名。改规则先改这里。
//! - [`db`]：连接的打开、体检、整库重建。**唯一**的构造入口是
//!   [`IndexDb::open`]，不存在「打开但没校验」的中间状态。
//! - [`repo`]：行类型与仓储方法（增量同步要用的读写、FTS 查询）。
//!
//! # 它不做什么
//!
//! 它**不**知道 Vault 长什么样、**不**读磁盘上的笔记、**不**做分词。
//! 正文到 token 的转换在 `ki-search`，扫描在 `ki-vault`。这里只负责「把已经
//! 算好的 token 存进去、按 token 查出来」。
//!
//! 唯一知道路径的地方是 [`IndexDb::db_path`]——索引库放在 Vault 内部，
//! 所以它必须知道 `<vault>/.knowisle/` 这个名字。

mod db;
mod repo;
mod schema;

pub use db::{IndexDb, OpenOutcome, RebuildReason};
pub use repo::{
    Candidate, Counts, IndexedNote, NoteBatch, NoteState, SkipReason, SkippedNote, SyncStats,
};
pub use schema::{DB_FILE, SCHEMA_VERSION, TOKENIZER_VERSION};

/// 测试夹具。放在 crate 里而不是 `tests/` 下，是因为它们要用 `IndexDb::conn()`
/// 这种 `pub(crate)` 的逃生口来伪造「坏状态」。
#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;

    use crate::{IndexDb, IndexedNote};

    /// 一个空 Vault 目录。`TempDir` 一 drop 就删，测试之间不会互相污染。
    pub(crate) fn vault() -> tempfile::TempDir {
        tempfile::tempdir().expect("创建临时目录")
    }

    /// 建好索引库的 Vault。返回 `TempDir` 是为了让它活着——drop 掉目录就没了。
    pub(crate) fn opened_vault() -> (tempfile::TempDir, IndexDb) {
        let dir = vault();
        let (db, _) = IndexDb::open(dir.path()).expect("打开索引库");
        (dir, db)
    }

    /// 造一篇「已算好 token」的笔记。这里不关心 token 怎么来的（那是
    /// `ki-search` 的事），只关心它能不能被原样存进去、原样查出来。
    ///
    /// 四个字符串都是**手写的**，刻意不调用 `ki-search`：单元测试要断言
    /// 「存什么查什么」，让夹具跟着分词器走会把这条判据变成同义反复。
    pub(crate) fn indexed(
        path: &str,
        body_ascii: &str,
        body_bigram: &str,
        body_tokens: &str,
    ) -> IndexedNote {
        IndexedNote {
            path: path.to_string(),
            name_lower: Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default(),
            size: body_ascii.len() as u64,
            mtime: 1_700_000_000,
            body_ascii: body_ascii.to_string(),
            body_bigram: body_bigram.to_string(),
            body_tokens: body_tokens.to_string(),
            src_encoding: None,
        }
    }

    /// 同上，但记成「磁盘上不是 UTF-8」。
    ///
    /// 单独一个构造函数而不是给 `indexed` 加参数：调用点有二十几处，
    /// 加参数会让每一处都多一个 `None`，而其中只有两处关心编码。
    pub(crate) fn indexed_gbk(
        path: &str,
        body_ascii: &str,
        body_bigram: &str,
        body_tokens: &str,
    ) -> IndexedNote {
        IndexedNote {
            src_encoding: Some("GBK".to_string()),
            ..indexed(path, body_ascii, body_bigram, body_tokens)
        }
    }
}
