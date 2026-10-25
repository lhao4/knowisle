//! 索引库的 schema：版本号、DDL、表名。
//!
//! # 为什么没有迁移（不用 refinery）
//!
//! 这个库里全是**派生数据**：删掉它，搜索功能仍然完整，只是第一次搜索要重建
//! （10k 篇约 5 秒）。迁移是给「删了就没了」的数据用的；对可重建的库，迁移
//! 只会留下一串立刻会被丢弃的迁移文件，并且引入一条「迁移写错了但没人发现」
//! 的新故障路径。
//!
//! 所以版本不符的处理是**整库重建**，判据见 [`crate::db::RebuildReason`]。
//!
//! 偿还触发条件：第一张存不可重建数据的表（历史版本、链接元数据）落地时，
//! 同时引入 refinery 与从 v1 起的基线迁移——那时重建不再是无损操作。

/// 当前 schema 版本，写进 `PRAGMA user_version`。
pub const SCHEMA_VERSION: i32 = 1;

/// 分词与投影规则的版本。
///
/// 它和 schema 版本是**两个**东西：schema 变了要重建，规则变了也要重建。
/// 混成一个数会让「只改了 bigram 规则」这种改动看起来像 schema 变更，
/// 排查时误导人。
///
/// 改动 [`crate::db::IndexDb`] 用到的任何切分/投影规则时必须同步改这个串，
/// 否则老库会带着旧 token 继续被查询，表现为「搜不到刚写的词」。
pub const TOKENIZER_VERSION: &str = "bigram-v1/ascii-proj-v1";

/// 索引库文件名（放在 `<vault>/.knowisle/` 下）。
pub const DB_FILE: &str = "index.sqlite";

/// 建库 DDL。
///
/// `note.id` 是 `INTEGER PRIMARY KEY`，也就是 rowid——两张 FTS 表的 `rowid`
/// 直接等于它，不需要映射表，也不需要 `content_rowid`（那是外部内容表才有的）。
///
/// 两张 FTS 表都是 `content=''`（不存原文，守住公理 A1）：
/// - 普通 `content=''` **不支持** `DELETE`（会报 `cannot DELETE from contentless
///   fts5 table`），所以必须带 `contentless_delete=1`（SQLite ≥ 3.43）。
/// - `contentless_delete=1` 与 `columnsize=0` **互斥**，一起写建表直接报错。
/// - 不要加 `detail=`：默认 `full` 是唯一稳妥的选择。
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS note (
  id          INTEGER PRIMARY KEY,
  path        TEXT    NOT NULL UNIQUE,
  name_lower  TEXT    NOT NULL,
  size        INTEGER NOT NULL,
  mtime       INTEGER NOT NULL,
  status      TEXT    NOT NULL DEFAULT 'ok' CHECK (status IN ('ok','skipped')),
  skip_reason TEXT,
  indexed_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS note_status ON note(status);

-- 通道 A：ASCII 投影，服务英文与代码标识符（词首前缀匹配）
CREATE VIRTUAL TABLE IF NOT EXISTS note_fts USING fts5(
  body_ascii,
  content='',
  contentless_delete=1,
  tokenize='unicode61 remove_diacritics 0'
);

-- 通道 B：中文 bigram + 跨脚本边界 bigram
CREATE VIRTUAL TABLE IF NOT EXISTS note_fts_zh USING fts5(
  body_tokens,
  content='',
  contentless_delete=1,
  tokenize='unicode61 remove_diacritics 0'
);
"#;

/// `meta` 表的键。字符串常量集中在这里，避免各处手写拼错。
pub mod meta_key {
    pub const SCHEMA_VERSION: &str = "schema_version";
    pub const TOKENIZER_VERSION: &str = "tokenizer_version";
    pub const VAULT_ROOT: &str = "vault_root";
    pub const SQLITE_VERSION: &str = "sqlite_version";
    pub const CREATED_AT: &str = "created_at";
    pub const SYNCED_AT: &str = "synced_at";
}
