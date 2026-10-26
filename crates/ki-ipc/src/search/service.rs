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

use ki_core::{KiError, ScanResult, SearchHit, SearchResult};
use ki_search::{fuse, plan, project, token_stream, Mode, Plan};
use ki_store::{
    Counts, IndexDb, IndexedNote, NoteState, OpenOutcome, SkipReason, SkippedNote, SyncStats,
};
use ki_vault::{
    flatten_files, fold_ascii, name_lower_for_path, read_note, scan_vault, FileRef, NoteLocator,
    ScanOptions, MAX_HITS, MAX_HITS_PER_FILE, MAX_INDEX_BYTES,
};

use super::cache::{canonical_root, ensure_root, lock};

/// 每批多少篇提交一次。
///
/// 256 是个折中：太小则提交次数多（10k 篇 = 40 次提交变成 100 次），
/// 太大则单批的读盘时间会明显拉长（用户在这期间看到的还是旧索引）。
const BATCH_SIZE: usize = 256;

/// 每条 FTS 通道取前多少篇。
const CHANNEL_LIMIT: usize = 200;

/// 文件名通道取前多少篇。它**全部进池**——`name_hit` 是排序契约的第一键，
/// 被截断挤掉会让「按文件名搜」直接失效。
const NAME_LIMIT: usize = 400;

/// 候选池上限。超过就只保留 RRF 分数最高的那些，并把结果标成 `truncated`。
const POOL_LIMIT: usize = 400;

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
                    note: indexed_note(&note),
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

// ---------------------------------------------------------------------------
// 查询路径
// ---------------------------------------------------------------------------

/// 用 FTS5 索引回答一次查询。
///
/// # 分工：索引选候选，定位器出结果
///
/// FTS5 只回答「哪些笔记值得看」；行号、UTF-16 列号、摘要窗口全部由
/// [`NoteLocator`] 的字面子串扫描产出。这条分工是整次换芯的支点：
///
/// - 18 条既有行为契约（一行一条、长行窗口化、跨行跳过…）自动继续成立，
///   因为它们是**同一份代码**在跑；
/// - 检索精度与旧引擎完全相同，差异只可能来自候选集；
/// - 不需要 `snippet()`（contentless 表本来也不支持），也不需要在库里存原文。
///
/// # 最终顺序仍由定位器决定
///
/// RRF 的分数只用来决定**谁进候选池**（池满 400 时）。命中行的最终顺序还是
/// `name_hit desc → path asc → line asc`，与旧引擎逐字一致——否则前端列表的
/// 顺序会变，而那是没有契约支撑的行为变更。
///
/// 相关性排序（让"最相关"的笔记排在前面）**仍然没有做**。它需要新的排序契约
/// 与前端文案，不属于「换实现」的范围。ADR-007 记了这条。
pub fn search(root: &Path, query: &str, refresh: Option<&str>) -> Result<SearchResult, KiError> {
    let root = canonical_root(root)?;
    let mut cache = lock();
    ensure_root(&mut cache, &root);

    let mut open_ms = 0u64;
    if cache.store.is_none() {
        let started = Instant::now();
        let (db, _) = IndexDb::open(&root)?;
        cache.store = Some(db);
        cache.builds += 1;
        open_ms = started.elapsed().as_millis() as u64;
    }

    // 取走并清零：这是「本次搜索之前最近一次索引维护耗时」，只该被报告一次。
    // 第二次搜索显示 0，与旧引擎「命中缓存时为 0」的语义一致。
    let pending_ms = std::mem::take(&mut cache.pending_index_ms);
    let builds = cache.builds;
    let db = cache.store.as_mut().expect("上面刚保证过 store 非空");

    // 索引库从没同步过 → 先扫一遍再搜。
    //
    // 旧引擎的 `query` 是自足的：没有索引就当场建一份。新引擎若不自足，
    // 「直接调 `vault_search` 而没先 `vault_scan`」会静默返回空结果——
    // 那是最糟的失败模式（不报错，只是找不到东西）。
    //
    // 代价：只有**全新索引库**会走这条路（`synced_at` 为 0）。真实使用里
    // 打开库一定先 `vault_scan`，所以这是给直接调用 IPC 与测试准备的保险。
    if db.synced_at()? == 0 {
        let started = Instant::now();
        let scan = scan_vault(&root, &ScanOptions::default())?;
        let now = now_ms();
        sync(db, &root, &scan, now)?;
        db.set_synced_at(now)?;
        open_ms += started.elapsed().as_millis() as u64;
    }

    if let Some(path) = refresh {
        // 刷新失败不该让搜索失败：用户要的是结果，不是错误。
        let _ = refresh_path(db, &root, path);
    }

    let counts = db.counts()?;
    let qplan = plan(query);

    let mut result = match qplan.mode {
        Mode::Empty => finish(Vec::new(), 0, counts, false),
        // 全扫：索引里没有可查的 token（单字、纯 emoji、孤立 CJK 单字），
        // 只能把已索引的笔记全部过一遍定位器。代价与旧引擎建索引时读一遍全库
        // 相当——不存正文就必须重新读，这是 contentless 的必然结果。
        Mode::FullScan => {
            let pool: Vec<String> = db
                .ok_candidates()?
                .into_iter()
                .map(|candidate| candidate.path)
                .collect();
            locate_all(&root, &pool, query, counts, false)
        }
        Mode::Ascii | Mode::Cjk => indexed_search(db, &root, query, &qplan, counts)?,
    };

    result.index_ms = pending_ms + open_ms;
    result.builds = builds;
    Ok(result)
}

/// 走 FTS 通道：候选 → RRF 融合 → 定位。
fn indexed_search(
    db: &IndexDb,
    root: &Path,
    query: &str,
    qplan: &Plan,
    counts: Counts,
) -> Result<SearchResult, KiError> {
    let channel = match qplan.mode {
        Mode::Ascii => db.match_ascii(&qplan.expr, CHANNEL_LIMIT)?,
        Mode::Cjk => db.match_zh(&qplan.expr, CHANNEL_LIMIT)?,
        _ => unreachable!("indexed_search 只处理 Ascii / Cjk"),
    };
    // 取满了就说明池可能被截断，`total` 从「准确值」降级为「下界」。
    let channel_saturated = channel.len() >= CHANNEL_LIMIT;

    // 文件名通道与 FTS 通道互不替代：`name_hit` 是排序契约的第一键，
    // 一篇文件名命中但正文没命中的笔记必须进池，否则「按文件名搜」直接失效。
    let needle = fold_ascii(query.trim());
    let names = if needle.is_empty() {
        Vec::new()
    } else {
        db.match_name(&needle, NAME_LIMIT)?
    };

    let fused = fuse(&[
        channel.iter().map(|c| c.id).collect(),
        names.iter().map(|c| c.id).collect(),
    ]);

    let mut paths: HashMap<i64, &str> = HashMap::with_capacity(channel.len() + names.len());
    for candidate in channel.iter().chain(names.iter()) {
        paths.insert(candidate.id, candidate.path.as_str());
    }

    let pool_saturated = channel_saturated || fused.len() > POOL_LIMIT;
    let pool: Vec<String> = fused
        .iter()
        .take(POOL_LIMIT)
        .filter_map(|hit| paths.get(&hit.id).map(|path| (*path).to_string()))
        .collect();

    Ok(locate_all(root, &pool, query, counts, pool_saturated))
}

/// 对候选池逐篇定位，产出最终结果。
///
/// 读不到的文件静默跳过：它可能在扫描与搜索之间被删掉了。**不要**因此把
/// 整次搜索变成错误——一篇被删的笔记不该让用户搜不了整个库。
fn locate_all(
    root: &Path,
    pool: &[String],
    query: &str,
    counts: Counts,
    pool_saturated: bool,
) -> SearchResult {
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut total = 0usize;

    for path in pool {
        let Ok(note) = read_note(root, path) else {
            continue;
        };
        let file_hits = NoteLocator::new(note.path, note.content).hits(query, MAX_HITS_PER_FILE);
        total += file_hits.len();
        hits.extend(file_hits);
    }

    finish(hits, total, counts, pool_saturated)
}

/// 排序、截断、填诊断字段。
///
/// 排序键与旧引擎**逐字相同**：文件名命中优先 → 路径 → 行号。三者都是确定的，
/// 所以结果与遍历顺序无关（候选池的顺序来自 RRF，但它不影响最终顺序）。
fn finish(
    mut hits: Vec<SearchHit>,
    total: usize,
    counts: Counts,
    pool_saturated: bool,
) -> SearchResult {
    hits.sort_by(|a, b| {
        b.name_hit
            .cmp(&a.name_hit)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });

    let truncated = hits.len() > MAX_HITS || pool_saturated;
    hits.truncate(MAX_HITS);

    SearchResult {
        hits,
        total,
        truncated,
        indexed_files: counts.ok,
        skipped_files: counts.skipped,
        index_ms: 0,
        builds: 0,
    }
}

/// 把一篇笔记**定点**写进索引，不重扫全库。
///
/// # 谁需要它
///
/// 回收站恢复（`trash_restore`）。文件回到了磁盘，但索引里没有它——而
/// 增量同步是挂在 `vault_scan` 后面的。若只靠前端「恢复完记得重扫一次」，
/// 那么任何绕过前端的调用（将来的 `isle` CLI、集成测试）都会得到
/// 「恢复回来了，但搜不到」：不报错，只是找不到。
///
/// # 为什么不是 `invalidate`
///
/// `invalidate` 只丢掉进程内的连接；索引库里缺的那一行还在缺。下一次搜索
/// 重开连接时 `synced_at` 非 0，不会自动补——必须真的写一行进去。
///
/// # 边界
///
/// - 索引库还没打开，或从没同步过（`synced_at == 0`）：什么都不做。
///   下一次搜索会自建并全量同步；在这里替它开工只会往一个空库里写一行。
/// - 读不到（刚又被删了、编码坏了、超过 2MB）：**删掉那一行**。超大文件
///   下次扫描会以 `skipped` 的身份回来，不值得为它多一条分支。
/// - 失败由调用方吞掉：恢复已经成功，索引问题不该让用户以为「没恢复」。
pub fn reindex(root: &Path, rel: &str) -> Result<(), KiError> {
    let root = canonical_root(root)?;
    let mut cache = lock();
    ensure_root(&mut cache, &root);

    let Some(db) = cache.store.as_mut() else {
        return Ok(());
    };
    if db.synced_at()? == 0 {
        return Ok(());
    }

    let now = now_ms();
    match read_note(&root, rel) {
        Ok(note) => {
            db.upsert_ok(&indexed_note(&note), now)?;
        }
        Err(_) => {
            db.delete_by_path(rel)?;
        }
    }
    Ok(())
}

/// 把「当前正在编辑的那一篇」重新索引一次。
///
/// 每次搜索只重读一个文件，换来的是「刚打的字立刻能搜到」。若改成整库重建，
/// 连打 3 个字符就是 3 次全库 IO；若完全不刷新，用户会认定「刚写的搜不到」
/// 是 bug。
///
/// 四种结果对应四种处置，与旧引擎的 `refresh` 一一对应：
///
/// | 磁盘状态 | 处置 |
/// |---|---|
/// | 内容没变（size + mtime 都没动） | 不写库 |
/// | 变了 / 上次是跳过的 | `upsert_ok` |
/// | 超过 2MB 上限 | `upsert_skipped(TooLarge)` |
/// | 读不到 | **删行**（读不到的不该继续贡献命中） |
fn refresh_path(db: &mut IndexDb, root: &Path, path: &str) -> Result<(), KiError> {
    // 不在库里就什么都不做：新文件要等下一次 `vault_scan` 才可见。
    // 这是 B-09 的既有边界，不在这里偷偷补一个"文件系统监听"。
    let Some(prev) = db.note_state(path)? else {
        return Ok(());
    };

    let now = now_ms();
    match read_note(root, path) {
        Ok(note) if note.size <= MAX_INDEX_BYTES => {
            if prev.ok && prev.size == note.size && prev.mtime == note.mtime {
                return Ok(());
            }
            db.upsert_ok(&indexed_note(&note), now)?;
        }
        Ok(note) => {
            db.upsert_skipped(
                &SkippedNote {
                    name_lower: name_lower_for_path(&note.path),
                    path: note.path,
                    size: note.size,
                    mtime: note.mtime,
                    reason: SkipReason::TooLarge,
                },
                now,
            )?;
        }
        Err(KiError::TooLarge { size, .. }) => {
            db.upsert_skipped(
                &SkippedNote {
                    path: path.to_string(),
                    name_lower: name_lower_for_path(path),
                    size,
                    // 读失败拿不到 mtime，沿用库里的值：下次扫描看到 size 变了
                    // 就会重试，而 size 没变时重试也没意义。
                    mtime: prev.mtime,
                    reason: SkipReason::TooLarge,
                },
                now,
            )?;
        }
        Err(KiError::InvalidEncoding { .. }) => {
            db.upsert_skipped(
                &SkippedNote {
                    path: path.to_string(),
                    name_lower: name_lower_for_path(path),
                    size: prev.size,
                    mtime: prev.mtime,
                    reason: SkipReason::InvalidUtf8,
                },
                now,
            )?;
        }
        Err(_) => {
            db.delete_by_path(path)?;
        }
    }
    Ok(())
}

/// 正文 → 待写库的行。同步与刷新共用，保证两边算出来的 token 完全一致。
fn indexed_note(note: &ki_core::NoteContent) -> IndexedNote {
    IndexedNote {
        path: note.path.clone(),
        name_lower: name_lower_for_path(&note.path),
        size: note.size,
        mtime: note.mtime,
        body_ascii: project(&note.content),
        body_tokens: token_stream(&note.content),
    }
}
