//! 扫描之后的增量同步：diff → 读盘 → 分批写库。
//!
//! # 为什么同步挂在 `vault_scan` 后面
//!
//! 没有文件监听（B-09）时，唯一的"磁盘变了"信号就是用户主动扫描。
//! 挂在扫描后面意味着：**索引的新鲜度与树的新鲜度是同一个东西**——
//! 用户看到树更新了，索引也就更新了。若另起一个定时器，两者会各自漂移，
//! 而漂移是不可见的（搜不到刚建的文件，没有任何提示）。
//!
//! # 判据是 `size + mtime`，不是内容哈希
//!
//! 哈希要读全文，那正是我们想省掉的那次 IO。`size + mtime` 的已知缺口是
//! 「同一毫秒内改动且字节数不变」——它由两件事兜底：
//!
//! 1. 用户正在编辑的那一篇走 `refresh`（每次搜索前重读），不依赖这个判据；
//! 2. 重新扫描会更新 mtime，而编辑器保存一定会改 mtime。
//!
//! 剩下真正漏掉的场景是"外部程序用 `utimes` 把 mtime 改回去了"，
//! 那已经超出「本地优先 Markdown 笔记库」该防御的范围。记在 BACKLOG 里。
//!
//! # 分批写入
//!
//! 每 256 篇一个事务。首次建 10k 篇的库时，一个大事务会让 WAL 长到全库大小；
//! 分批的语义代价只是"崩溃时可能只写进前 N 批"，而那种中间态会被下次打开
//! 时的行数体检发现并重建——不影响正确性。
//!
//! # IO 预算
//!
//! 0 变化的一次同步 = 0 字节正文 + 1 次 `SELECT` + 1 次 `commit`（+10–20ms）。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ki_core::{KiError, ScanResult};
use ki_search::{project, token_stream};
use ki_store::{IndexDb, IndexedNote, NoteState, OpenOutcome, SkipReason, SkippedNote, SyncStats};
use ki_vault::{flatten_files, name_lower_for_path, read_note, FileRef, MAX_INDEX_BYTES};

use super::cache::{canonical_root, ensure_root, lock};

/// 每批多少篇提交一次。
///
/// 256 是个折中：太小则提交次数多（10k 篇 = 40 次提交变成 100 次），
/// 太大则单批的读盘时间会明显拉长（用户在这期间看到的还是旧索引）。
const BATCH_SIZE: usize = 256;

/// 一次同步的结果。
#[derive(Debug, Clone)]
pub struct SyncReport {
    pub stats: SyncStats,
    /// 本次调用是否新建 / 重建了索引库。`None` 表示复用了已经打开的连接。
    ///
    /// 重建之后所有文件都会重新读一遍——调用方（基准脚本、诊断）靠它解释
    /// 「为什么这次同步读了 30MB」。
    pub opened: Option<OpenOutcome>,
}

/// 扫描之后把索引对齐到磁盘。
///
/// 调用方必须**吞掉**这个错误：用户点「选择文件夹」要的是树，
/// 索引维护失败不该让树出不来。失败时搜索会退化成"搜不到新内容"，
/// 而那是可自愈的（下次扫描重试）。
pub fn sync_after_scan(root: &Path, scan: &ScanResult) -> Result<SyncReport, KiError> {
    let started = Instant::now();
    let root = canonical_root(root)?;
    let mut cache = lock();
    ensure_root(&mut cache, &root);

    let opened = if cache.store.is_none() {
        let (db, outcome) = IndexDb::open(&root)?;
        cache.store = Some(db);
        Some(outcome)
    } else {
        None
    };

    let now = now_ms();
    let stats = {
        let db = cache.store.as_mut().expect("上面刚保证过 store 非空");
        let stats = sync(db, &root, scan, now)?;
        db.set_synced_at(now)?;
        stats
    };

    // 累计而不是覆盖：一次 `vault_scan` 之后可能还有几次 `refresh`，
    // 它们都属于"本次搜索之前最近一次索引维护"。
    cache.pending_index_ms += started.elapsed().as_millis() as u64;

    Ok(SyncReport { stats, opened })
}

/// 一次同步要做的一件事。
enum Job {
    /// 读这篇（新文件，或者 size/mtime 变了）。
    Write(FileRef),
    /// 从索引里删掉这篇。
    Remove(String),
}

/// 已经读好、只等写库的一项。读盘在事务外完成，事务里只有 SQL。
enum Prepared {
    Index { note: IndexedNote, existed: bool },
    Skip(SkippedNote),
    Remove(String),
}

fn sync(db: &mut IndexDb, root: &Path, scan: &ScanResult, now: i64) -> Result<SyncStats, KiError> {
    let started = Instant::now();
    let mut stats = SyncStats::default();

    let mut files = flatten_files(&scan.tree);
    // 扫描器的顺序是「目录优先 + 名字不区分大小写」；同步只要一个确定性顺序，
    // 这样两批之间的分界在同样的输入下总是一样的（便于复现问题）。
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let state = db.load_state()?;
    let known: HashMap<&str, &NoteState> = state.iter().map(|s| (s.path.as_str(), s)).collect();

    let mut jobs: Vec<Job> = Vec::new();
    for file in &files {
        match known.get(file.path.as_str()) {
            // `size` 与 `mtime` 都没变 → 不读内容。**跳过的也不重试**：
            // 一个 3MB 的文件在它被改小之前，重试只是白白读 3MB。
            Some(prev) if prev.size == file.size && prev.mtime == file.mtime => {
                stats.unchanged += 1;
            }
            _ => jobs.push(Job::Write(file.clone())),
        }
    }

    // `scan.truncated` 时**绝不删**：扫描在文件数上限处截断，缺失的路径
    // 只代表"没扫到"，不代表"文件没了"。拿截断结果做删除会把上限之后的
    // 全部笔记从索引里抹掉——而且下一次搜索会静默地少掉一大批结果。
    if !scan.truncated {
        let live: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
        for prev in &state {
            if !live.contains(prev.path.as_str()) {
                jobs.push(Job::Remove(prev.path.clone()));
            }
        }
    }

    for chunk in jobs.chunks(BATCH_SIZE) {
        // 阶段一：读盘与分词（事务外）。这里是最慢的一段，不能占着写锁。
        let prepared = prepare(chunk, root, &known, &mut stats)?;

        // 阶段二：写库（一个事务）。任何一条失败都整批回滚。
        db.batch(|batch| {
            for item in &prepared {
                match item {
                    Prepared::Index { note, existed } => {
                        batch.upsert_ok(note, now)?;
                        if *existed {
                            stats.updated += 1;
                        } else {
                            stats.inserted += 1;
                        }
                    }
                    Prepared::Skip(note) => {
                        batch.upsert_skipped(note, now)?;
                    }
                    Prepared::Remove(path) => {
                        if batch.delete_by_path(path)? {
                            stats.deleted += 1;
                        }
                    }
                }
            }
            Ok(())
        })?;
    }

    stats.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(stats)
}

/// 把一批任务变成"只等写库"的形态。
///
/// 单篇失败**不连坐**：非 UTF-8、超大、IO 失败的条目只记成 `skipped` 并计数。
/// 一条坏文件让整次同步报错，等于用户放了一个 GBK 文件就再也建不了索引。
fn prepare(
    chunk: &[Job],
    root: &Path,
    known: &HashMap<&str, &NoteState>,
    stats: &mut SyncStats,
) -> Result<Vec<Prepared>, KiError> {
    let mut out = Vec::with_capacity(chunk.len());

    for job in chunk {
        let file = match job {
            Job::Remove(path) => {
                out.push(Prepared::Remove(path.clone()));
                continue;
            }
            Job::Write(file) => file,
        };

        let existed = known.contains_key(file.path.as_str());

        // 先用扫描时拿到的 size 挡掉超大文件：省一次 open + read。
        if file.size > MAX_INDEX_BYTES {
            stats.skipped += 1;
            out.push(Prepared::Skip(skipped_note(file, SkipReason::TooLarge)));
            continue;
        }

        match read_note(root, &file.path) {
            Ok(note) => {
                // 用 `read_note` 返回的 size/mtime，而不是扫描时的值：
                // 扫描与读取之间文件可能变了，存**实际索引的那一版**，
                // 下次扫描才不会误判成"未变"而漏掉这次改动。
                stats.bytes_read += note.size;
                out.push(Prepared::Index {
                    note: IndexedNote {
                        path: note.path.clone(),
                        name_lower: name_lower_for_path(&note.path),
                        size: note.size,
                        mtime: note.mtime,
                        body_ascii: project(&note.content),
                        body_tokens: token_stream(&note.content),
                    },
                    existed,
                });
            }
            Err(KiError::TooLarge { .. }) => {
                stats.skipped += 1;
                out.push(Prepared::Skip(skipped_note(file, SkipReason::TooLarge)));
            }
            Err(KiError::InvalidEncoding { .. }) => {
                stats.skipped += 1;
                out.push(Prepared::Skip(skipped_note(file, SkipReason::InvalidUtf8)));
            }
            Err(_) => {
                stats.skipped += 1;
                out.push(Prepared::Skip(skipped_note(file, SkipReason::Io)));
            }
        }
    }

    Ok(out)
}

/// 跳过的笔记用**扫描时**的 size/mtime 落库。
///
/// 这样下次扫描看到同样的 size/mtime 就判"未变"，不会为一个 3MB 的文件
/// 反复读盘。文件一旦被改动，size 或 mtime 必然变，届时自然会重试。
fn skipped_note(file: &FileRef, reason: SkipReason) -> SkippedNote {
    SkippedNote {
        path: file.path.clone(),
        name_lower: name_lower_for_path(&file.path),
        size: file.size,
        mtime: file.mtime,
        reason,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
