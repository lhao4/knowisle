//! 索引库的行类型与仓储方法。
//!
//! 这些类型**不进 IPC**，所以不 derive `specta::Type`——它们只在 `ki-store`
//! 与 `ki-ipc` 之间流动。跨 IPC 的检索结果仍然是 `ki_core::SearchHit`。
//!
//! # 为什么手写 SQL 而不是 ORM
//!
//! 这里只有 6 条语句，而 FTS5 的 `MATCH`/`bm25()` 本来就得手写。引入 ORM 换来
//! 的是「多一层看不见的 SQL」和「联表时 bm25 拿不到正确上下文」这两个问题。
//!
//! # 每个方法都是「内层返回 `rusqlite::Result`，外层映射一次错误」
//!
//! 否则每个 `?` 都要 `.map_err(|e| index_err(path, &e))`，六个方法写下来就是
//! 三十行噪音，而且很容易漏掉一处，让 `KiError::Index` 和 `rusqlite::Error`
//! 混在一个返回类型里。

use std::fmt;
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use ki_core::KiError;

use crate::db::{index_err, IndexDb};
use crate::schema::meta_key;

/// 一篇成功索引的笔记要写库的东西。
///
/// `body_ascii` / `body_tokens` 是**派生数据**（从正文算出来的 token 流），
/// 不是原文副本：原文永远只在磁盘上（公理 A1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedNote {
    /// Vault 相对路径（POSIX）。
    pub path: String,
    /// 去掉扩展名后的文件名，ASCII 折叠。文件名命中走独立的精确通道。
    pub name_lower: String,
    pub size: u64,
    pub mtime: i64,
    /// 通道 A：ASCII 投影后的文本。
    pub body_ascii: String,
    /// 通道 B：中文 bigram，空格连接。
    pub body_tokens: String,
}

/// 一篇索引失败的笔记。仍然写进 `note` 表，让计数与「为什么跳过」可见，
/// 但两张 FTS 表里没有它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedNote {
    pub path: String,
    pub name_lower: String,
    pub size: u64,
    pub mtime: i64,
    pub reason: SkipReason,
}

/// 跳过原因。**只用于诊断展示**，不参与检索逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 超过单篇索引上限（2MB）。
    TooLarge,
    /// 不是合法 UTF-8。
    InvalidUtf8,
    /// 读取时 IO 失败。
    Io,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::TooLarge => "too_large",
            SkipReason::InvalidUtf8 => "invalid_utf8",
            SkipReason::Io => "io",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `note` 表里一行的同步状态。增量同步靠 `size + mtime` 判断「要不要重读」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteState {
    pub id: i64,
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    /// `status == 'ok'`。`false` 表示上次索引时跳过了它。
    pub ok: bool,
}

/// 候选笔记。FTS 只回答「哪些笔记」，行级定位仍由 `ki-vault` 的字面扫描器做。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: i64,
    pub path: String,
    pub name_lower: String,
}

/// `note` 表的计数，用来填 `SearchResult` 的 `indexed_files` / `skipped_files`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub ok: usize,
    pub skipped: usize,
}

/// 一次增量同步的统计。全部是诊断字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncStats {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub skipped: usize,
    /// 实际从磁盘读到的正文字节数。**第二次同步必须是 0**，
    /// 这是「增量真的增量」的唯一硬证据。
    pub bytes_read: u64,
    pub elapsed_ms: u64,
}

impl IndexDb {
    /// 读出全部行的同步状态。
    ///
    /// 同步层拿它和磁盘扫描结果做差集，决定「哪些要读、哪些要删、哪些跳过」。
    /// 一次全表扫描，10k 行约 2ms——比逐篇 `stat` 便宜得多。
    pub fn load_state(&self) -> Result<Vec<NoteState>, KiError> {
        fn inner(conn: &Connection) -> rusqlite::Result<Vec<NoteState>> {
            let mut stmt =
                conn.prepare_cached("SELECT id, path, size, mtime, status = 'ok' FROM note")?;
            let rows = stmt.query_map([], |r| {
                Ok(NoteState {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    // SQLite 的 INTEGER 是有符号的；size 存进去时已经钳到 i64::MAX，
                    // 读回来再转 u64 不可能溢出。
                    size: r.get::<_, i64>(2)? as u64,
                    mtime: r.get(3)?,
                    ok: r.get(4)?,
                })
            })?;
            rows.collect()
        }
        inner(self.conn()).map_err(|e| index_err(self.path(), &e))
    }

    /// `note` 表的计数，用来填 `SearchResult` 的 `indexed_files` / `skipped_files`。
    pub fn counts(&self) -> Result<Counts, KiError> {
        fn inner(conn: &Connection) -> rusqlite::Result<Counts> {
            conn.query_row(
                "SELECT (SELECT count(*) FROM note WHERE status = 'ok'),
                        (SELECT count(*) FROM note WHERE status = 'skipped')",
                [],
                |r| {
                    Ok(Counts {
                        ok: r.get::<_, i64>(0)? as usize,
                        skipped: r.get::<_, i64>(1)? as usize,
                    })
                },
            )
        }
        inner(self.conn()).map_err(|e| index_err(self.path(), &e))
    }

    /// 写入（或覆盖）一篇成功索引的笔记，返回它的 rowid。
    ///
    /// 两张 FTS 表都是 contentless：**不能 UPDATE**，内容变了只能删了重插。
    /// 这也是为什么这里是一个事务而不是三条独立语句——中间崩掉会留下
    /// 「note 有了、FTS 没有」的行，那正是体检里 [`RowCountMismatch`] 要抓的
    /// 状态，虽然能自愈，但不该由我们主动制造。
    ///
    /// [`RowCountMismatch`]: crate::RebuildReason::RowCountMismatch
    pub fn upsert_ok(&mut self, note: &IndexedNote, now: i64) -> Result<i64, KiError> {
        self.in_tx(|conn| upsert_ok_sql(conn, note, now))
    }

    /// 写入（或覆盖）一篇索引失败的笔记。
    ///
    /// 它仍然占一行，让「为什么搜不到这篇」在计数与诊断里可见；但如果它**上次**
    /// 是成功的（比如刚长过 2MB 上限），必须把两张 FTS 表里的旧行删掉——
    /// 否则会留下「正文已不可索引，却还能被搜出来」的幽灵命中。
    pub fn upsert_skipped(&mut self, note: &SkippedNote, now: i64) -> Result<i64, KiError> {
        self.in_tx(|conn| upsert_skipped_sql(conn, note, now))
    }

    /// 删掉一篇笔记（含两张 FTS 表里的行）。返回它之前是否存在。
    pub fn delete_by_path(&mut self, path: &str) -> Result<bool, KiError> {
        self.in_tx(|conn| delete_by_path_sql(conn, path))
    }

    /// 单条写入：一个事务一次提交。
    ///
    /// 只在「刷新当前编辑篇」这类单篇场景用它。批量同步必须走 [`IndexDb::batch`]，
    /// 否则 10k 篇就是 10k 次提交。
    fn in_tx<T>(
        &mut self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, KiError> {
        let conn = self.conn();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| index_err(self.path(), &e))?;
        let out = f(&tx).map_err(|e| index_err(self.path(), &e))?;
        tx.commit().map_err(|e| index_err(self.path(), &e))?;
        Ok(out)
    }

    /// 批量写：整批共用一个事务，由调用方决定「多少篇一批」。
    ///
    /// 为什么要分批而不是一个大事务：首次建 10k 篇的库时，一个大事务会让
    /// WAL 一直长到全库大小、内存里的页缓存也一直涨；而每 256 篇提交一次
    /// 的代价只是 40 次提交。分批的**语义**代价是「崩溃时可能只写进前 N 批」，
    /// 但那只是一种 `RowCountMismatch` 之外的中间态——体检会在下次打开时
    /// 发现行数不一致并重建，所以不影响正确性。
    ///
    /// `f` 返回 `Err` 时整批回滚（`tx` 随 drop 回滚），不会留下半批。
    pub fn batch<T>(
        &mut self,
        f: impl FnOnce(&mut NoteBatch<'_>) -> Result<T, KiError>,
    ) -> Result<T, KiError> {
        let conn = self.conn();
        let path = self.path();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| index_err(path, &e))?;
        let mut batch = NoteBatch { conn: &tx, path };
        let out = f(&mut batch)?;
        tx.commit().map_err(|e| index_err(path, &e))?;
        Ok(out)
    }

    /// 读一篇笔记的同步状态。不存在返回 `None`。
    ///
    /// 「刷新当前编辑篇」每次搜索都要问一次，所以不能复用 [`IndexDb::load_state`]——
    /// 那是全表扫描，10k 行 2ms，而这条走 `path` 的唯一索引。
    pub fn note_state(&self, path: &str) -> Result<Option<NoteState>, KiError> {
        fn inner(conn: &Connection, path: &str) -> rusqlite::Result<Option<NoteState>> {
            conn.query_row(
                "SELECT id, path, size, mtime, status = 'ok' FROM note WHERE path = ?1",
                [path],
                |r| {
                    Ok(NoteState {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        size: r.get::<_, i64>(2)? as u64,
                        mtime: r.get(3)?,
                        ok: r.get(4)?,
                    })
                },
            )
            .optional()
        }
        inner(self.conn(), path).map_err(|e| index_err(self.path(), &e))
    }

    /// 全部已索引成功的笔记，按路径升序。
    ///
    /// 全扫模式（单字查询、纯 emoji 查询）用它当候选池。**这是唯一会拉全表的
    /// 查询**，代价与旧引擎建索引时读一遍全库相当——不存正文就必须重新读，
    /// 这是 contentless 的必然结果（公理 A1 要求索引里没有第二份真相）。
    pub fn ok_candidates(&self) -> Result<Vec<Candidate>, KiError> {
        fn inner(conn: &Connection) -> rusqlite::Result<Vec<Candidate>> {
            let mut stmt = conn.prepare_cached(
                "SELECT id, path, name_lower FROM note WHERE status = 'ok' ORDER BY path",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(Candidate {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    name_lower: r.get(2)?,
                })
            })?;
            rows.collect()
        }
        inner(self.conn()).map_err(|e| index_err(self.path(), &e))
    }

    /// 通道 A（ASCII 投影）查询。`expr` 必须是**已经拼好的 FTS5 表达式**，
    /// 语法由 `ki-search::plan` 负责生成。
    ///
    /// 空表达式直接返回空集：`MATCH ''` 会抛 `fts5: syntax error`。
    pub fn match_ascii(&self, expr: &str, limit: usize) -> Result<Vec<Candidate>, KiError> {
        self.match_fts("note_fts", expr, limit)
    }

    /// 通道 B（中文 bigram）查询。
    pub fn match_zh(&self, expr: &str, limit: usize) -> Result<Vec<Candidate>, KiError> {
        self.match_fts("note_fts_zh", expr, limit)
    }

    fn match_fts(&self, table: &str, expr: &str, limit: usize) -> Result<Vec<Candidate>, KiError> {
        if expr.is_empty() {
            return Ok(Vec::new());
        }
        fn inner(
            conn: &Connection,
            table: &str,
            expr: &str,
            limit: usize,
        ) -> rusqlite::Result<Vec<Candidate>> {
            // `table` 只可能是本文件里写死的两个常量。**绝不允许**把它换成参数：
            // SQLite 的表名不能参数化，而这个 `format!` 是唯一能注入的位置。
            //
            // 另外注意这里用的是真表名而不是别名：FTS5 的 `MATCH` 与 `bm25()`
            // 都要求左操作数是表的**名字**，写 `FROM note_fts f ... WHERE f MATCH ?`
            // 会报 `no such column: f`。
            let sql = format!(
                "SELECT n.id, n.path, n.name_lower
                 FROM {table} JOIN note n ON n.id = {table}.rowid
                 WHERE {table} MATCH ?1 AND n.status = 'ok'
                 ORDER BY bm25({table})
                 LIMIT ?2"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![expr, size_of_usize(limit)], |r| {
                Ok(Candidate {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    name_lower: r.get(2)?,
                })
            })?;
            rows.collect()
        }
        inner(self.conn(), table, expr, limit).map_err(|e| index_err(self.path(), &e))
    }

    /// 文件名通道：`name_lower` 的字面子串匹配。
    ///
    /// 文件名不参与 FTS：它短、要求精确、而且 `foo-bar.md` 这种带连字符的名字
    /// 过 `unicode61` 会被拆成两个 token，反而搜不到。
    pub fn match_name(&self, needle: &str, limit: usize) -> Result<Vec<Candidate>, KiError> {
        fn inner(
            conn: &Connection,
            needle: &str,
            limit: usize,
        ) -> rusqlite::Result<Vec<Candidate>> {
            let pattern = format!("%{}%", escape_like(needle));
            let mut stmt = conn.prepare_cached(
                "SELECT id, path, name_lower FROM note
                 WHERE status = 'ok' AND name_lower LIKE ?1 ESCAPE '\\'
                 ORDER BY length(name_lower), name_lower
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![pattern, size_of_usize(limit)], |r| {
                Ok(Candidate {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    name_lower: r.get(2)?,
                })
            })?;
            rows.collect()
        }
        inner(self.conn(), needle, limit).map_err(|e| index_err(self.path(), &e))
    }

    /// 记录「上一次成功同步的时间」。仅用于诊断。
    pub fn set_synced_at(&self, now: i64) -> Result<(), KiError> {
        fn inner(conn: &Connection, now: i64) -> rusqlite::Result<()> {
            conn.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
                params![meta_key::SYNCED_AT, now.to_string()],
            )?;
            Ok(())
        }
        inner(self.conn(), now).map_err(|e| index_err(self.path(), &e))
    }

    /// 上一次成功同步的时间（Unix 秒），从未同步过是 0。
    pub fn synced_at(&self) -> Result<i64, KiError> {
        fn inner(conn: &Connection) -> rusqlite::Result<i64> {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    [meta_key::SYNCED_AT],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(raw.and_then(|s| s.parse().ok()).unwrap_or(0))
        }
        inner(self.conn()).map_err(|e| index_err(self.path(), &e))
    }
}

/// 一批写入的句柄。
///
/// 方法都**不开事务**——事务由 [`IndexDb::batch`] 统一持有。这样同一条 SQL
/// 只有一份实现，单条写入与批量写入走的是同一段代码。
pub struct NoteBatch<'a> {
    conn: &'a Connection,
    /// 出错时要写进 `KiError::Index` 的路径。
    path: &'a Path,
}

impl NoteBatch<'_> {
    pub fn upsert_ok(&mut self, note: &IndexedNote, now: i64) -> Result<i64, KiError> {
        upsert_ok_sql(self.conn, note, now).map_err(|e| index_err(self.path, &e))
    }

    pub fn upsert_skipped(&mut self, note: &SkippedNote, now: i64) -> Result<i64, KiError> {
        upsert_skipped_sql(self.conn, note, now).map_err(|e| index_err(self.path, &e))
    }

    pub fn delete_by_path(&mut self, path: &str) -> Result<bool, KiError> {
        delete_by_path_sql(self.conn, path).map_err(|e| index_err(self.path, &e))
    }
}

fn upsert_ok_sql(conn: &Connection, note: &IndexedNote, now: i64) -> rusqlite::Result<i64> {
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM note WHERE path = ?1", [&note.path], |r| {
            r.get(0)
        })
        .optional()?;

    let id = match existing {
        Some(id) => {
            conn.execute(
                "UPDATE note SET name_lower = ?2, size = ?3, mtime = ?4,
                                 status = 'ok', skip_reason = NULL, indexed_at = ?5
                 WHERE id = ?1",
                params![id, note.name_lower, size_of(note.size), note.mtime, now],
            )?;
            conn.execute("DELETE FROM note_fts WHERE rowid = ?1", [id])?;
            conn.execute("DELETE FROM note_fts_zh WHERE rowid = ?1", [id])?;
            id
        }
        None => {
            conn.execute(
                "INSERT INTO note(path, name_lower, size, mtime, status, indexed_at)
                 VALUES(?1, ?2, ?3, ?4, 'ok', ?5)",
                params![
                    note.path,
                    note.name_lower,
                    size_of(note.size),
                    note.mtime,
                    now
                ],
            )?;
            conn.last_insert_rowid()
        }
    };

    conn.execute(
        "INSERT INTO note_fts(rowid, body_ascii) VALUES(?1, ?2)",
        params![id, note.body_ascii],
    )?;
    conn.execute(
        "INSERT INTO note_fts_zh(rowid, body_tokens) VALUES(?1, ?2)",
        params![id, note.body_tokens],
    )?;
    Ok(id)
}

fn upsert_skipped_sql(conn: &Connection, note: &SkippedNote, now: i64) -> rusqlite::Result<i64> {
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM note WHERE path = ?1", [&note.path], |r| {
            r.get(0)
        })
        .optional()?;

    let id = match existing {
        Some(id) => {
            conn.execute(
                "UPDATE note SET name_lower = ?2, size = ?3, mtime = ?4,
                                 status = 'skipped', skip_reason = ?5, indexed_at = ?6
                 WHERE id = ?1",
                params![
                    id,
                    note.name_lower,
                    size_of(note.size),
                    note.mtime,
                    note.reason.as_str(),
                    now
                ],
            )?;
            conn.execute("DELETE FROM note_fts WHERE rowid = ?1", [id])?;
            conn.execute("DELETE FROM note_fts_zh WHERE rowid = ?1", [id])?;
            id
        }
        None => {
            conn.execute(
                "INSERT INTO note(path, name_lower, size, mtime, status, skip_reason, indexed_at)
                 VALUES(?1, ?2, ?3, ?4, 'skipped', ?5, ?6)",
                params![
                    note.path,
                    note.name_lower,
                    size_of(note.size),
                    note.mtime,
                    note.reason.as_str(),
                    now
                ],
            )?;
            conn.last_insert_rowid()
        }
    };
    Ok(id)
}

fn delete_by_path_sql(conn: &Connection, path: &str) -> rusqlite::Result<bool> {
    let id: Option<i64> = conn
        .query_row("SELECT id FROM note WHERE path = ?1", [path], |r| r.get(0))
        .optional()?;
    let Some(id) = id else {
        return Ok(false);
    };
    conn.execute("DELETE FROM note_fts WHERE rowid = ?1", [id])?;
    conn.execute("DELETE FROM note_fts_zh WHERE rowid = ?1", [id])?;
    conn.execute("DELETE FROM note WHERE id = ?1", [id])?;
    Ok(true)
}

/// `u64 → i64` 的安全转换。
///
/// SQLite 的 INTEGER 是有符号 64 位。文件大小超过 8 EiB 在物理上不可能，
/// 但 `as` 转换在溢出时会静默回绕成负数，而负数会让「mtime+size 未变」的
/// 比较永远为假——表现为每次同步都全量重读。钳到 `i64::MAX` 至少是单调的。
fn size_of(size: u64) -> i64 {
    i64::try_from(size).unwrap_or(i64::MAX)
}

fn size_of_usize(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// 转义 `LIKE` 的通配符。
///
/// 不转义的话，搜 `100%` 会变成「以 100 开头」，搜 `a_b` 会匹配 `axb`。
/// 用户输入的 `%` 在文件名通道里就是一个普通字符。
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{indexed, opened_vault};

    const NOW: i64 = 1_700_000_100;

    /// 写进去 → 两条通道都能查出来。
    #[test]
    fn upsert_then_match_both_channels() {
        let (_dir, mut db) = opened_vault();
        let id = db
            .upsert_ok(
                &indexed("notes/rust.md", "the rust book", "the ru us st bo oo ok"),
                NOW,
            )
            .expect("写入");
        assert!(id > 0);

        let ascii = db.match_ascii("\"rust\"", 10).expect("查 ASCII");
        assert_eq!(ascii.len(), 1);
        assert_eq!(ascii[0].path, "notes/rust.md");
        assert_eq!(ascii[0].id, id);

        let zh = db.match_zh("\"us\" AND \"st\"", 10).expect("查中文通道");
        assert_eq!(zh.len(), 1);
        assert_eq!(zh[0].path, "notes/rust.md");
    }

    /// 覆盖同一路径时必须把旧 token 删干净（contentless 表不能 UPDATE）。
    #[test]
    fn upsert_overwrites_old_tokens() {
        let (_dir, mut db) = opened_vault();
        db.upsert_ok(&indexed("a.md", "alpha bravo", "al lp ph ha"), NOW)
            .expect("首次写入");
        db.upsert_ok(&indexed("a.md", "charlie delta", "ch ha ar"), NOW)
            .expect("覆盖写入");

        assert!(db.match_ascii("\"alpha\"", 10).expect("查旧词").is_empty());
        assert_eq!(db.match_ascii("\"charlie\"", 10).expect("查新词").len(), 1);
        // 同一个路径不能变成两行
        assert_eq!(db.load_state().expect("读状态").len(), 1);
    }

    /// 跳过一篇笔记时，如果它上次是成功的，旧的 FTS 行必须消失——
    /// 否则会出现「正文已不可索引，却还能被搜出来」的幽灵命中。
    #[test]
    fn upsert_skipped_drops_previous_tokens() {
        let (_dir, mut db) = opened_vault();
        db.upsert_ok(&indexed("big.md", "alpha", "al lp"), NOW)
            .expect("先成功索引");

        let skipped = SkippedNote {
            path: "big.md".to_string(),
            name_lower: "big".to_string(),
            size: 3 * 1024 * 1024,
            mtime: 1_700_000_200,
            reason: SkipReason::TooLarge,
        };
        db.upsert_skipped(&skipped, NOW).expect("改为跳过");

        assert!(
            db.match_ascii("\"alpha\"", 10).expect("查旧词").is_empty(),
            "跳过之后不该还能搜到正文"
        );
        let counts = db.counts().expect("读计数");
        assert_eq!((counts.ok, counts.skipped), (0, 1));
    }

    /// 删除要同时清掉 note 行与两张 FTS 表里的行，否则行数体检会一直失败。
    #[test]
    fn delete_by_path_removes_from_both_fts() {
        let (_dir, mut db) = opened_vault();
        db.upsert_ok(&indexed("a.md", "alpha", "al lp"), NOW)
            .expect("写入");

        assert!(db.delete_by_path("a.md").expect("删除"));
        assert!(db.match_ascii("\"alpha\"", 10).expect("查").is_empty());
        assert!(db.match_zh("\"al\"", 10).expect("查").is_empty());
        assert!(db.load_state().expect("读状态").is_empty());
        assert_eq!(db.counts().expect("读计数"), Counts::default());

        // 幂等：再删一次返回 false，不报错
        assert!(!db.delete_by_path("a.md").expect("重复删除"));
    }

    /// `load_state` 要把「跳过」的行也读出来——同步层靠它判断「这篇上次失败了，
    /// 但 mtime 没变，这次还要不要再试」。
    #[test]
    fn load_state_reports_skip_status() {
        let (_dir, mut db) = opened_vault();
        db.upsert_ok(&indexed("a.md", "alpha", "al lp"), NOW)
            .expect("写成功行");
        db.upsert_skipped(
            &SkippedNote {
                path: "b.md".to_string(),
                name_lower: "b".to_string(),
                size: 9,
                mtime: 42,
                reason: SkipReason::InvalidUtf8,
            },
            NOW,
        )
        .expect("写跳过行");

        let mut state = db.load_state().expect("读状态");
        state.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(state.len(), 2);
        assert_eq!((state[0].path.as_str(), state[0].ok), ("a.md", true));
        assert_eq!((state[1].path.as_str(), state[1].ok), ("b.md", false));
        assert_eq!(state[1].mtime, 42);
    }

    /// 文件名通道是字面子串匹配，`%` / `_` 必须当普通字符。
    #[test]
    fn match_name_escapes_like_wildcards() {
        let (_dir, mut db) = opened_vault();
        db.upsert_ok(&indexed("a/100%.md", "", ""), NOW)
            .expect("写");
        db.upsert_ok(&indexed("a/1000.md", "", ""), NOW)
            .expect("写");
        db.upsert_ok(&indexed("a/a_b.md", "", ""), NOW).expect("写");
        db.upsert_ok(&indexed("a/axb.md", "", ""), NOW).expect("写");

        let hit = db.match_name("100%", 10).expect("查 %");
        assert_eq!(hit.len(), 1, "`%` 必须当字面字符，不能当通配符");
        assert_eq!(hit[0].path, "a/100%.md");

        let hit = db.match_name("a_b", 10).expect("查 _");
        assert_eq!(hit.len(), 1, "`_` 必须当字面字符");
        assert_eq!(hit[0].path, "a/a_b.md");
    }

    /// 空表达式必须返回空集：`MATCH ''` 在 SQLite 里是语法错误，不是空结果。
    #[test]
    fn empty_expr_returns_empty_set() {
        let (_dir, mut db) = opened_vault();
        db.upsert_ok(&indexed("a.md", "alpha", "al lp"), NOW)
            .expect("写");
        assert!(db.match_ascii("", 10).expect("空表达式").is_empty());
        assert!(db.match_zh("", 10).expect("空表达式").is_empty());
    }

    /// 跳过的笔记不该出现在任何查询结果里（`status = 'ok'` 过滤）。
    #[test]
    fn skipped_notes_are_invisible_to_queries() {
        let (_dir, mut db) = opened_vault();
        db.upsert_skipped(
            &SkippedNote {
                path: "big.md".to_string(),
                name_lower: "big".to_string(),
                size: 1,
                mtime: 1,
                reason: SkipReason::TooLarge,
            },
            NOW,
        )
        .expect("写跳过行");

        assert!(db.match_name("big", 10).expect("查文件名").is_empty());
    }

    /// `synced_at` 往返。
    #[test]
    fn synced_at_roundtrip() {
        let (_dir, db) = opened_vault();
        assert_eq!(db.synced_at().expect("初值"), 0);
        db.set_synced_at(NOW).expect("写入");
        assert_eq!(db.synced_at().expect("读回"), NOW);
    }

    /// 批量写：整批一个事务，写完三条都在。
    #[test]
    fn batch_writes_many_rows_in_one_transaction() {
        let (_dir, mut db) = opened_vault();
        let notes: Vec<IndexedNote> = (0..8)
            .map(|i| indexed(&format!("n{i}.md"), "alpha", "al lp"))
            .collect();

        db.batch(|batch| {
            for note in &notes {
                batch.upsert_ok(note, NOW)?;
            }
            Ok(())
        })
        .expect("批量写入");

        assert_eq!(db.counts().expect("读计数").ok, 8);
        assert_eq!(db.match_ascii("\"alpha\"", 100).expect("查").len(), 8);
    }

    /// 批次里任一条失败 → 整批回滚，不留半批。
    ///
    /// 这是「分 256 篇一批」的安全前提：崩溃或错误只会丢掉**整批**，
    /// 不会留下「note 有了、FTS 没有」的行。
    #[test]
    fn batch_error_rolls_back_the_whole_batch() {
        let (_dir, mut db) = opened_vault();

        let result: Result<(), KiError> = db.batch(|batch| {
            batch.upsert_ok(&indexed("a.md", "alpha", "al lp"), NOW)?;
            batch.upsert_ok(&indexed("b.md", "bravo", "br ra"), NOW)?;
            Err(KiError::Index {
                path: "simulated".to_string(),
                message: "模拟第 3 篇读取失败".to_string(),
            })
        });

        assert!(result.is_err());
        assert_eq!(db.counts().expect("读计数"), Counts::default());
        assert!(db.match_ascii("\"alpha\"", 10).expect("查").is_empty());
    }
}
