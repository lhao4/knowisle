//! 索引库的连接生命周期：打开、体检、整库重建。
//!
//! # 为什么「重建」是默认的故障处置
//!
//! 这个库里全是派生数据（公理 A1）。任何可疑状态——页结构损坏、schema 版本
//! 不符、分词规则换了、行数对不上——最省事也最安全的处置都是**整库重建**：
//! 代价是第一次搜索要重新读一遍 Vault（10k 篇约 5 秒），收益是不必写一条
//! 「从坏状态 A 修到状态 B」的修复路径，那条路径没人会去测，而它会一直留在
//! 代码里等着在某台机器上做错事。
//!
//! 所以 [`IndexDb::open`] 的返回值里带着 [`OpenOutcome`]：调用方（和测试）能
//! 分辨「新建」「复用」「重建」，而不是被蒙在鼓里。

use std::ffi::OsString;
use std::io::ErrorKind as IoErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ki_core::{KiError, METADATA_DIR};
use rusqlite::{Connection, ErrorCode};

use crate::schema::{meta_key, DB_FILE, DDL, SCHEMA_VERSION, TOKENIZER_VERSION};

/// 删库文件失败时的重试次数。
///
/// 3 次、退避 50/100/150ms。占用通常来自「上一个句柄刚关到一半」或者杀毒软件
/// 正在扫这个新文件，都是几十毫秒级的事；退避到秒级只会让用户盯着白屏。
const PURGE_ATTEMPTS: u32 = 3;

/// SQLite 的 busy 超时。
///
/// 索引库只有一个写入者（本进程），这个值纯粹是为「上一次崩溃留下的锁还没被
/// 操作系统回收」这种瞬间准备的。设成 5 秒是因为再长也救不回来——真被另一个
/// 进程占着，用户该看到的是错误而不是转圈。
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// 一次 [`IndexDb::open`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenOutcome {
    /// 这个 Vault 第一次建索引库。
    Created,
    /// 现有索引库体检通过，直接复用。
    Reused,
    /// 现有索引库不可用，已经整库重建。
    Rebuilt(RebuildReason),
}

impl OpenOutcome {
    /// 是否发生过重建。同步层用它决定要不要无条件全量扫。
    pub fn was_rebuilt(&self) -> bool {
        matches!(self, OpenOutcome::Rebuilt(_))
    }
}

/// 触发整库重建的具体原因。**全部是诊断字段**，只用于日志与测试断言，
/// 不参与任何控制流——处置动作永远只有一个：重建。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildReason {
    /// 页结构损坏，或者根本不是 SQLite 文件。
    Corrupt { detail: String },
    /// `PRAGMA user_version` 与 [`SCHEMA_VERSION`] 不符。
    SchemaVersion { found: i32 },
    /// 分词/投影规则换过，旧 token 不能再被查询命中。
    TokenizerVersion { found: Option<String> },
    /// 这个索引库属于另一个 Vault（`.knowisle/` 被整目录复制过来的典型后果）。
    VaultRoot { found: Option<String> },
    /// `note(status='ok')` 与三张 FTS 表的行数对不上，说明上次同步中途崩了。
    RowCountMismatch {
        notes: i64,
        ascii: i64,
        bigram: i64,
        zh: i64,
    },
}

impl RebuildReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RebuildReason::Corrupt { .. } => "corrupt",
            RebuildReason::SchemaVersion { .. } => "schema_version",
            RebuildReason::TokenizerVersion { .. } => "tokenizer_version",
            RebuildReason::VaultRoot { .. } => "vault_root",
            RebuildReason::RowCountMismatch { .. } => "row_count_mismatch",
        }
    }
}

impl std::fmt::Display for RebuildReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 打开索引库时的中间判定。
enum Verdict {
    /// 空文件 / 没有 `note` 表：还没建过。
    Fresh,
    /// 体检全过。
    Reused,
    Rebuild(RebuildReason),
}

/// 第一次尝试打开的结果。
enum Probe {
    /// 文件是空的（或根本没有 `note` 表）：需要建表，不需要删文件。
    Fresh(Connection),
    Ready(Connection, OpenOutcome),
    Rebuild(Option<Connection>, RebuildReason),
}

/// 索引库连接。
///
/// 持有连接本身就意味着「已经体检过、可以直接用」——构造函数只有一个，
/// 不存在「打开但没校验」的中间状态。
pub struct IndexDb {
    conn: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for IndexDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexDb").field("path", &self.path).finish()
    }
}

impl IndexDb {
    /// 索引库在某个 Vault 里的固定位置。
    ///
    /// 放在 `<vault>/.knowisle/` 而不是用户数据目录，是为了「把整个文件夹拷到
    /// 另一台机器」时索引跟着走——虽然它会因为 `vault_root` 变化而重建，
    /// 但至少不会出现「库在这、索引在别处、换个 Vault 就串味」的情况。
    pub fn db_path(root: &Path) -> PathBuf {
        root.join(METADATA_DIR).join(DB_FILE)
    }

    /// 打开（必要时创建、体检、重建）某个 Vault 的索引库。
    ///
    /// 顺序：打开 → 每连接 PRAGMA → 体检 → 合格就复用，不合格就删掉重来。
    pub fn open(root: &Path) -> Result<(IndexDb, OpenOutcome), KiError> {
        let dir = root.join(METADATA_DIR);
        std::fs::create_dir_all(&dir)
            .map_err(|e| KiError::io_msg(dir.to_string_lossy(), &e, "无法创建索引目录"))?;
        let path = dir.join(DB_FILE);

        match probe(&path, root)? {
            Probe::Fresh(conn) => {
                // 「新建」与「复用」的差别只有这一步：空文件要先建表。
                bootstrap(&conn, root).map_err(|e| index_err(&path, &e))?;
                Ok((IndexDb { conn, path }, OpenOutcome::Created))
            }
            Probe::Ready(conn, outcome) => Ok((IndexDb { conn, path }, outcome)),
            Probe::Rebuild(conn, reason) => {
                // Windows 上不 drop 就删不掉：SQLite 还开着这个文件的句柄。
                drop(conn);
                purge(&path)?;
                let conn = init(&path, root)?;
                Ok((IndexDb { conn, path }, OpenOutcome::Rebuilt(reason)))
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 裸连接。写操作要开事务时用它（`Connection` 的方法全是 `&self`，
    /// 所以只读访问权就够开 `unchecked_transaction`）。
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// 打开 + 体检。**不**做重建——重建需要先 drop 连接，那是 [`IndexDb::open`] 的事。
fn probe(path: &Path, root: &Path) -> Result<Probe, KiError> {
    let conn = match Connection::open(path) {
        Ok(c) => c,
        Err(e) if is_corruption(&e) => {
            return Ok(Probe::Rebuild(
                None,
                RebuildReason::Corrupt {
                    detail: e.to_string(),
                },
            ))
        }
        Err(e) => return Err(index_err(path, &e)),
    };

    // 每连接 PRAGMA 必须在体检之前：损坏的库可能连 journal_mode 都读不出来，
    // 那本身就是一条「该重建」的证据。
    if let Err(e) = apply_conn_pragmas(&conn) {
        if is_corruption(&e) {
            return Ok(Probe::Rebuild(
                Some(conn),
                RebuildReason::Corrupt {
                    detail: e.to_string(),
                },
            ));
        }
        return Err(index_err(path, &e));
    }

    match inspect(&conn, root) {
        Ok(Verdict::Fresh) => Ok(Probe::Fresh(conn)),
        Ok(Verdict::Reused) => Ok(Probe::Ready(conn, OpenOutcome::Reused)),
        Ok(Verdict::Rebuild(reason)) => Ok(Probe::Rebuild(Some(conn), reason)),
        Err(e) if is_corruption(&e) => Ok(Probe::Rebuild(
            Some(conn),
            RebuildReason::Corrupt {
                detail: e.to_string(),
            },
        )),
        Err(e) => Err(index_err(path, &e)),
    }
}

/// 体检。返回 `Rebuild` 表示「能用但不可信」，返回 `Err` 表示「读不动」。
fn inspect(conn: &Connection, root: &Path) -> rusqlite::Result<Verdict> {
    // 1. quick_check(1) 而不是 integrity_check。
    //    integrity_check 会逐行逐字段校验，10k 篇的库上要几百毫秒；
    //    quick_check 只查页结构与索引一致性，够回答「能不能安全地读」。
    //    参数 1 = 最多报告 1 条错误，我们只要知道「不是 ok」。
    let quick: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if quick != "ok" {
        return Ok(Verdict::Rebuild(RebuildReason::Corrupt { detail: quick }));
    }

    // 2. 表在不在。**不能**用 user_version 判断是否首次：全新文件也是 0。
    let has_note: bool = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'note'",
        [],
        |r| r.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if !has_note {
        return Ok(Verdict::Fresh);
    }

    // 3. schema 版本。
    let user_version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if user_version != SCHEMA_VERSION {
        return Ok(Verdict::Rebuild(RebuildReason::SchemaVersion {
            found: user_version,
        }));
    }

    // 4. 分词规则版本。规则变了而库没变，表现是「搜不到刚写的词」——
    //    最容易被当成检索 bug 查半天，所以必须显式对版本。
    let tokenizer = read_meta(conn, meta_key::TOKENIZER_VERSION)?;
    if tokenizer.as_deref() != Some(TOKENIZER_VERSION) {
        return Ok(Verdict::Rebuild(RebuildReason::TokenizerVersion {
            found: tokenizer,
        }));
    }

    // 5. 这个库是不是属于这个 Vault。
    let vault = read_meta(conn, meta_key::VAULT_ROOT)?;
    let want = vault_root_key(root);
    if vault.as_deref() != Some(want.as_str()) {
        return Ok(Verdict::Rebuild(RebuildReason::VaultRoot { found: vault }));
    }

    // 6. 四张表的行数必须相等。不等说明上次同步在「写了 note 还没写 FTS」
    //    或者「删了 FTS 没删 note」的半路上崩了，增量逻辑会从此算错。
    //
    //    三张 FTS 表都在这里对账，而不是只对两张：`note_fts_ascii_bi` 漏一行
    //    的表现是「某个子串查询少一篇」，不报错、不可见——正是最该被体检抓到的
    //    那种失败。
    let (notes, ascii, bigram, zh) = conn.query_row(
        "SELECT (SELECT count(*) FROM note WHERE status = 'ok'),
                (SELECT count(*) FROM note_fts),
                (SELECT count(*) FROM note_fts_ascii_bi),
                (SELECT count(*) FROM note_fts_zh)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if notes != ascii || notes != bigram || notes != zh {
        return Ok(Verdict::Rebuild(RebuildReason::RowCountMismatch {
            notes,
            ascii,
            bigram,
            zh,
        }));
    }

    Ok(Verdict::Reused)
}

fn read_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()
}

/// 每连接（非持久）的 PRAGMA。
fn apply_conn_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // WAL 下 synchronous=NORMAL 不会损坏数据库，最坏情况是断电丢最后一个事务。
    // 而索引库丢了最后一个事务的后果是「重建」，也就是回到同步前的状态。
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// 新建索引库：PRAGMA → DDL → 版本与来源信息。
///
/// 全部工作包在一个 `rusqlite::Result` 里，这样外面只需要映射一次错误。
fn init(path: &Path, root: &Path) -> Result<Connection, KiError> {
    let conn = Connection::open(path).map_err(|e| index_err(path, &e))?;
    bootstrap(&conn, root).map_err(|e| index_err(path, &e))?;
    Ok(conn)
}

fn bootstrap(conn: &Connection, root: &Path) -> rusqlite::Result<()> {
    apply_conn_pragmas(conn)?;

    // journal_mode 会返回一行结果，用 `execute` 会报 "Execute returned results"。
    // 返回值在 WAL 不可用的文件系统上可能是 "delete"（网络盘、某些只读挂载），
    // 那不是错误，继续用回滚日志即可。
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;

    conn.execute_batch(DDL)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

    let now = now_secs();
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt =
            tx.prepare_cached("INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)")?;
        for (key, value) in [
            (meta_key::SCHEMA_VERSION, SCHEMA_VERSION.to_string()),
            (meta_key::TOKENIZER_VERSION, TOKENIZER_VERSION.to_string()),
            (meta_key::VAULT_ROOT, vault_root_key(root)),
            (meta_key::SQLITE_VERSION, rusqlite::version().to_string()),
            (meta_key::CREATED_AT, now.to_string()),
            (meta_key::SYNCED_AT, "0".to_string()),
        ] {
            stmt.execute([key, value.as_str()])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Vault 根目录的规范字符串，用来判断索引库属于谁。
///
/// 用 `dunce::canonicalize` 而不是 `std::fs::canonicalize`：后者在 Windows 上会
/// 返回 `\\?\E:\...` 的 verbatim 形式，同一个目录写出来的字符串会随调用方式变化。
///
/// 解析失败（目录刚被删、权限不足）就退回原样字符串：宁可因为字符串不一致多
/// 重建一次，也不要把「拿不到规范路径」升级成打开失败。
fn vault_root_key(root: &Path) -> String {
    match dunce::canonicalize(root) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => root.to_string_lossy().into_owned(),
    }
}

/// 删掉索引库的三个文件。**只**删这三个，绝不删目录。
fn purge(path: &Path) -> Result<(), KiError> {
    let mut last: Option<std::io::Error> = None;

    for attempt in 1..=PURGE_ATTEMPTS {
        match remove_db_files(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
            }
        }
    }

    // 删不掉就改名。语义是「把它挪开」，新库仍然建在同一个路径上。
    // 改名也失败才是真的没法自愈，此时唯一的出路是用户手动删文件，
    // 所以错误信息必须把完整路径写出来。
    let bak = bak_path(path);
    if std::fs::rename(path, &bak).is_ok() {
        // -wal / -shm 挪不走就删；删不掉也无所谓——新库的 WAL 盐值与旧的不同，
        // SQLite 打开时会直接忽略它。
        let _ = std::fs::remove_file(with_suffix(path, "-wal"));
        let _ = std::fs::remove_file(with_suffix(path, "-shm"));
        return Ok(());
    }

    let detail = last
        .map(|e| e.to_string())
        .unwrap_or_else(|| "未知错误".to_string());
    Err(KiError::Index {
        path: path.display().to_string(),
        message: format!("索引库被占用且无法改名，请手动删除该文件后重启：{detail}"),
    })
}

fn remove_db_files(path: &Path) -> std::io::Result<()> {
    for p in [
        path.to_path_buf(),
        with_suffix(path, "-wal"),
        with_suffix(path, "-shm"),
    ] {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == IoErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn bak_path(path: &Path) -> PathBuf {
    with_suffix(path, &format!(".bak-{}", now_secs()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 这个错误是不是「文件本身不可信」。
///
/// `DatabaseCorrupt`（11）与 `NotADatabase`（26）都归到这里：前者是页结构坏了，
/// 后者是「根本不是 SQLite 文件」。对调用方来说处置完全一样。
fn is_corruption(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

pub(crate) fn index_err(path: &Path, err: &rusqlite::Error) -> KiError {
    KiError::Index {
        path: path.display().to_string(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{opened_vault, vault};

    /// 建库必须真的建出目录、文件，且报「新建」。
    #[test]
    fn fresh_open_creates_schema() {
        let dir = vault();
        let path = IndexDb::db_path(dir.path());
        assert!(!path.exists());

        let (db, outcome) = IndexDb::open(dir.path()).expect("打开索引库");
        assert_eq!(outcome, OpenOutcome::Created);
        assert!(!outcome.was_rebuilt());
        assert!(path.exists(), "索引文件必须落在 .knowisle/ 下");
        assert_eq!(db.path(), path);
    }

    /// 第二次打开什么都不该重建——否则每次启动都要重读整个 Vault。
    #[test]
    fn reopen_reuses_index() {
        let dir = vault();
        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        drop(db);

        let (_db, outcome) = IndexDb::open(dir.path()).expect("再次打开");
        assert_eq!(outcome, OpenOutcome::Reused);
    }

    /// `journal_mode = WAL` 是持久设置，写在文件头里。
    #[test]
    fn journal_mode_is_wal() {
        let (_dir, db) = opened_vault();
        let mode: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("读 journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");
    }

    /// `bundled` 的 SQLite 必须带 FTS5，且版本够新到支持 `contentless_delete`。
    ///
    /// 这两件事都是**编译期特性**，只能在真库上验：编不过就是编不过，
    /// 但「编过了却少个开关」要到运行时插入第一篇文章才炸。
    #[test]
    fn bundled_sqlite_has_fts5_and_contentless_delete() {
        assert!(
            rusqlite::version_number() >= 3_43_00,
            "contentless_delete 需要 SQLite >= 3.43，当前 {}",
            rusqlite::version()
        );

        let (_dir, db) = opened_vault();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO note(path, name_lower, size, mtime, indexed_at)
             VALUES('x.md', 'x', 1, 1, 1)",
            [],
        )
        .expect("写 note");
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO note_fts(rowid, body_ascii) VALUES(?1, 'hello world')",
            [id],
        )
        .expect("写 FTS");
        // 没有 contentless_delete=1 的话，这一句会报
        // `cannot DELETE from contentless fts5 table`。
        conn.execute("DELETE FROM note_fts WHERE rowid = ?1", [id])
            .expect("contentless 表必须支持 DELETE");
    }

    /// contentless 表不存原文（公理 A1）：读回来必须是 NULL。
    ///
    /// 这条断言是「索引库不是第二份真相源」的机械保证——哪天有人把
    /// `content=''` 去掉，这里会立刻红。
    #[test]
    fn contentless_fts_does_not_store_body() {
        let (_dir, db) = opened_vault();
        let conn = db.conn();
        conn.execute(
            "INSERT INTO note(path, name_lower, size, mtime, indexed_at)
             VALUES('x.md', 'x', 1, 1, 1)",
            [],
        )
        .expect("写 note");
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO note_fts(rowid, body_ascii) VALUES(?1, 'secret body')",
            [id],
        )
        .expect("写 FTS");

        let stored: Option<String> = conn
            .query_row(
                "SELECT body_ascii FROM note_fts WHERE rowid = ?1",
                [id],
                |r| r.get(0),
            )
            .expect("读 FTS 列");
        assert_eq!(stored, None, "contentless 表不许留下正文副本");
    }

    /// schema 版本不符 → 重建，并且带上「读到的旧版本号」。
    #[test]
    fn schema_version_mismatch_rebuilds() {
        let dir = vault();
        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        db.conn()
            .pragma_update(None, "user_version", 999)
            .expect("改版本号");
        drop(db);

        let (db, outcome) = IndexDb::open(dir.path()).expect("再次打开");
        assert_eq!(
            outcome,
            OpenOutcome::Rebuilt(RebuildReason::SchemaVersion { found: 999 })
        );
        let version: i32 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("读版本号");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// 分词规则换过 → 重建。否则旧 token 会一直留在库里，表现为「搜不到刚写的词」。
    #[test]
    fn tokenizer_version_mismatch_rebuilds() {
        let dir = vault();
        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        db.conn()
            .execute(
                "UPDATE meta SET value = 'bigram-v0' WHERE key = ?1",
                [meta_key::TOKENIZER_VERSION],
            )
            .expect("改分词版本");
        drop(db);

        let (_db, outcome) = IndexDb::open(dir.path()).expect("再次打开");
        assert_eq!(
            outcome,
            OpenOutcome::Rebuilt(RebuildReason::TokenizerVersion {
                found: Some("bigram-v0".to_string())
            })
        );
    }

    /// 索引库属于另一个 Vault（`.knowisle/` 被整目录拷过来的典型后果）→ 重建。
    #[test]
    fn vault_root_mismatch_rebuilds() {
        let dir = vault();
        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        db.conn()
            .execute(
                "UPDATE meta SET value = '/some/other/vault' WHERE key = ?1",
                [meta_key::VAULT_ROOT],
            )
            .expect("改 vault_root");
        drop(db);

        let (_db, outcome) = IndexDb::open(dir.path()).expect("再次打开");
        assert_eq!(
            outcome,
            OpenOutcome::Rebuilt(RebuildReason::VaultRoot {
                found: Some("/some/other/vault".to_string())
            })
        );
    }

    /// 行数对不上（上次同步在「写了 note 还没写 FTS」的半路崩了）→ 重建。
    #[test]
    fn row_count_mismatch_rebuilds() {
        let dir = vault();
        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        db.conn()
            .execute(
                "INSERT INTO note(path, name_lower, size, mtime, status, indexed_at)
                 VALUES('ghost.md', 'ghost', 1, 1, 'ok', 1)",
                [],
            )
            .expect("插入没有 FTS 行的笔记");
        drop(db);

        let (_db, outcome) = IndexDb::open(dir.path()).expect("再次打开");
        assert_eq!(
            outcome,
            OpenOutcome::Rebuilt(RebuildReason::RowCountMismatch {
                notes: 1,
                ascii: 0,
                bigram: 0,
                zh: 0
            })
        );
    }

    /// 只有 `note_fts_ascii_bi` 少行也要重建。
    ///
    /// 这是本刀新增的那张表，漏一行的表现是「某个子串查询少一篇」——不报错、
    /// 不可见。三张表一起对账，才让这条失败模式在打开库时就被抓住。
    #[test]
    fn ascii_bigram_row_count_mismatch_rebuilds() {
        let dir = vault();
        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        db.conn()
            .execute(
                "INSERT INTO note(path, name_lower, size, mtime, status, indexed_at)
                 VALUES('ghost.md', 'ghost', 1, 1, 'ok', 1)",
                [],
            )
            .expect("插入笔记行");
        db.conn()
            .execute(
                "INSERT INTO note_fts(rowid, body_ascii) VALUES(1, 'ghost')",
                [],
            )
            .expect("写通道 A");
        db.conn()
            .execute(
                "INSERT INTO note_fts_zh(rowid, body_tokens) VALUES(1, 'gh')",
                [],
            )
            .expect("写通道 B");
        // 通道 A' 刻意不写。
        drop(db);

        let (_db, outcome) = IndexDb::open(dir.path()).expect("再次打开");
        assert_eq!(
            outcome,
            OpenOutcome::Rebuilt(RebuildReason::RowCountMismatch {
                notes: 1,
                ascii: 1,
                bigram: 0,
                zh: 1
            })
        );
    }

    /// 文件被写坏 → 重建，而不是把损坏往上抛成「索引库不可用」。
    #[test]
    fn corrupt_database_rebuilds() {
        let dir = vault();
        let path = IndexDb::db_path(dir.path());
        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        drop(db);

        // 必须是合法长度（>=100 字节）的非 SQLite 内容：0 字节会被当成空库。
        std::fs::write(&path, vec![b'x'; 4096]).expect("写坏文件");

        let (_db, outcome) = IndexDb::open(dir.path()).expect("损坏后仍能打开");
        assert!(
            matches!(outcome, OpenOutcome::Rebuilt(RebuildReason::Corrupt { .. })),
            "应当是 Corrupt，实际 {outcome:?}"
        );
    }

    /// 重建要把 `-wal` / `-shm` 一起清掉，否则残留的旧 WAL 会被当成新库的。
    #[test]
    fn rebuild_purges_sidecar_files() {
        let dir = vault();
        let path = IndexDb::db_path(dir.path());
        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        drop(db);

        let wal = with_suffix(&path, "-wal");
        std::fs::write(&path, vec![b'x'; 4096]).expect("写坏主文件");
        std::fs::write(&wal, b"STALE-WAL-MARKER").expect("伪造残留 WAL");

        let (_db, outcome) = IndexDb::open(dir.path()).expect("重建");
        assert!(outcome.was_rebuilt());
        let after = std::fs::read(&wal).unwrap_or_default();
        assert!(
            !after.starts_with(b"STALE-WAL"),
            "残留 WAL 必须被清掉，实际 {:?}",
            String::from_utf8_lossy(&after)
        );
    }

    /// 删掉索引文件不能碰 Vault 里的任何东西，重开后照常工作。
    #[test]
    fn deleting_index_file_leaves_vault_intact() {
        let dir = vault();
        let note = dir.path().join("a.md");
        std::fs::write(&note, "# 标题\n\n正文").expect("写笔记");

        let (db, _) = IndexDb::open(dir.path()).expect("首次打开");
        drop(db);

        std::fs::remove_file(IndexDb::db_path(dir.path())).expect("删索引");
        assert!(note.exists(), "索引是派生数据，删它不该影响笔记");

        let (db, outcome) = IndexDb::open(dir.path()).expect("重建");
        assert_eq!(outcome, OpenOutcome::Created);
        assert_eq!(db.counts().expect("读计数").ok, 0);
    }

    /// 错误必须是 `KiError::Index`，不是 `Io`：前端对它的建议是「删掉索引」，
    /// 而不是「改权限 / 关掉占用程序」。
    #[test]
    fn unusable_path_reports_index_error() {
        let dir = vault();
        let path = IndexDb::db_path(dir.path());
        std::fs::create_dir_all(&path).expect("用目录占住索引文件的位置");

        match IndexDb::open(dir.path()) {
            Err(KiError::Index { path: reported, .. }) => {
                assert_eq!(reported, path.display().to_string());
            }
            other => panic!("应当是 Index 错误，实际 {other:?}"),
        }
    }
}
