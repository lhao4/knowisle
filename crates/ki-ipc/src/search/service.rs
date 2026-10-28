//! 扫描之后的增量同步：diff → 读盘 → 分批写库。
//!
//! # 两个入口，一个内核
//!
//! | 入口 | 谁用 | 返回时机 |
//! |---|---|---|
//! | [`sync_after_scan`] | 基准、集成测试 | 索引建完才返回 |
//! | [`spawn_after_scan`] | `vault_scan`（生产路径） | 立刻返回，索引在后台建 |
//!
//! 内核是同一个 `sync`，所以两条路的写入结果**逐字节相同**——这是
//! 「语义零变化」的来源，也是基准能继续当判据的原因。
//!
//! # 为什么生产路径改成后台
//!
//! 同步挂在 `vault_scan` 后面时，树要等索引建完才出来：10k 库首次打开是
//! 「转圈 9.9 秒（缓存热）到 97.9 秒（缓存冷）」。用户点「选择文件夹」要的是
//! 那棵树，索引只是搜索的派生数据。
//!
//! 代价是「搜索时索引可能不全」。这件事**必须可见**（[`IndexState`]）——
//! 没有可见性的后台索引等于一个静默的不一致：搜不到刚扫描进来的文件，
//! 而界面上没有任何东西说明"还在建"。那正是 B-09 的坑。
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
//! 分批的语义代价只是"崩溃时可能只写进前 N 批"，而那种中间态会被下次扫描
//! 用 `size + mtime` 补齐——不影响正确性（`tests/background_index.rs` 有
//! 一条用例钉住它）。
//!
//! # IO 预算
//!
//! 0 变化的一次同步 = 0 字节正文 + 1 次 `SELECT` + 1 次 `commit`（+10–20ms）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::PoisonError;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ki_core::{IndexState, KiError, ScanResult, SearchHit, SearchResult};
use ki_search::{fuse, plan, project, token_stream, Mode, Plan};
use ki_store::{
    Counts, IndexDb, IndexedNote, NoteState, OpenOutcome, SkipReason, SkippedNote, SyncStats,
};
use ki_vault::{
    flatten_files, name_lower_for_path, read_note, read_note_in, scan_vault, FileRef, Located,
    NoteLocator, QueryNeedles, ResolvedVault, ScanOptions, MAX_HITS, MAX_HITS_PER_FILE,
    MAX_INDEX_BYTES,
};

use super::cache::{
    add_index_ms, bump_generation, canonical_root, ensure_root, generation, lock, publish,
    state_for,
};

/// 每批多少篇提交一次。
///
/// 256 是个折中：太小则提交次数多（10k 篇 = 40 次提交变成 100 次），
/// 太大则单批的读盘时间会明显拉长（用户在这期间看到的还是旧索引）。
const BATCH_SIZE: usize = 256;

/// 候选池的**精确预算**：候选不超过这个数时，全部定位，`total` 是准确值。
///
/// # 为什么是一个文件数，而不是"查询宽度"
///
/// 试过按查询的 token 数（bigram 数）自适应，实测**预测不了命中面**：
/// `样本笔记` 与 `本地优先` 都是 3 个 bigram，前者命中 1000/1000 篇，
/// 后者在 10k 库上命中 10000/10000 篇——同一个宽度，命中面可以差一个数量级。
/// 真正的判据是「索引说有多少篇可能命中」，它必须**查一次才知道**，
/// 而查它的代价只是一次索引扫描（不读正文）。所以预算按候选数给，
/// 取候选时多取一条用来判「有没有取满」。
///
/// # 1200 是怎么来的
///
/// 它必须盖住「小库里的宽查询」：1000 篇的库里，一个命中全部笔记的词
/// （`样本笔记`）要 1000 篇才数得准，验收就是这么钉的。1200 给夹具留了
/// 余量，同时把**精确路径的最坏代价**钉在 1200 篇上——与库多大无关。
/// 10k 库上这个上界的代价见 `POOL_SCAN_MAX` 的说明。
const POOL_EXACT_MAX: usize = 1200;

/// 候选超过精确预算时只定位这么多篇。
///
/// 此时 `total` **必然是下界**（`truncated = true`，UI 说「至少」），
/// 多读几篇只会让下界从 200 变成 400，对用户没有信息量；而少读一篇省一次
/// 磁盘 open。取 200 还有一条硬理由：`hits` 本来就截断到 `MAX_HITS = 200`，
/// 一篇至少贡献一条命中时，200 篇刚好能把结果列表填满。
const POOL_SCAN_MAX: usize = 200;

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

/// 扫描之后把索引对齐到磁盘，**同步**跑完。
///
/// 调用方必须**吞掉**这个错误：用户点「选择文件夹」要的是树，
/// 索引维护失败不该让树出不来。失败时搜索会退化成"搜不到新内容"，
/// 而那是可自愈的（下次扫描重试）。
///
/// # 它为什么还留着
///
/// 命令层已经改调 [`spawn_after_scan`]（树先出来）。这个函数保留同步语义，
/// 因为还有两个消费者需要「建完才返回」这个确定时点：
///
/// - 基准（`tests/index_bench.rs`）：耗时必须能被重复测量；
/// - 集成测试：不依赖线程调度就能断言索引内容。
///
/// **不要**把它们改成异步——那会让「建库耗时」变成一个测不准的量。
pub fn sync_after_scan(root: &Path, scan: &ScanResult) -> Result<SyncReport, KiError> {
    let started = Instant::now();
    let root = canonical_root(root)?;
    let files = sorted_files(scan);

    let mut cache = lock();
    ensure_root(&mut cache, &root);

    let opened = if cache.store.is_none() {
        let (db, outcome) = IndexDb::open(&root)?;
        cache.store = Some(db);
        Some(outcome)
    } else {
        None
    };

    // 先记代际：同步跑到一半时用户可能又点了一次「选择文件夹」。那时代际
    // 已经变了，这里**不许**把 Ready 发布到新一代头上。
    let generation = generation();
    let now = now_ms();
    let (stats, aborted) = {
        let db = cache.store.as_mut().expect("上面刚保证过 store 非空");
        let (stats, aborted) = sync(db, &root, &files, scan.truncated, now, None)?.into_parts();
        if !aborted {
            db.set_synced_at(now)?;
        }
        (stats, aborted)
    };

    // 累计而不是覆盖：一次 `vault_scan` 之后可能还有几次 `refresh`，
    // 它们都属于"本次搜索之前最近一次索引维护"。
    cache.pending_index_ms += started.elapsed().as_millis() as u64;
    drop(cache);

    if !aborted {
        publish(&root, generation, IndexState::Ready);
    }

    Ok(SyncReport { stats, opened })
}

/// 扫描之后把索引对齐到磁盘，**后台**跑：树先返回，索引慢慢建。
///
/// # 三条必须同时成立的约束（少一个就是 B-09 的坑）
///
/// 1. **树先出来**：本函数立刻返回，扫描结果已经被命令层拿走；
/// 2. **不全必须可见**：返回前就把状态置成 [`IndexState::Building`]，
///    之后每次 `vault_search` 都带着它回来，前端据此说"结果可能不全"；
/// 3. **切库立刻失效**：代际 +1，旧线程在下一批写库之前发现自己是旧的，
///    退出并释放连接（见 `cache::GENERATION`）。
///
/// # 为什么线程自己开连接
///
/// 见 `cache` 模块头：持 `CACHE` 跑 100 秒会把「首屏阻塞」换成「搜索阻塞」。
/// 代价是 `IndexDb::open` 的体检做两次（GUI 一次、后台一次）——10k 库上是
/// 几十毫秒，换的是整段建库期间搜索都不排队。**接受它**，别为省这一次
/// 去改 `IndexDb`。
///
/// # 失败不往上抛
///
/// 没有人接：树已经返回了。失败置 [`IndexState::Failed`]（用户看得见），
/// 索引本身仍可读——只是不全。下次扫描会重试。
pub fn spawn_after_scan(root: &Path, scan: &ScanResult) {
    let generation = bump_generation();

    let root = match canonical_root(root) {
        Ok(root) => root,
        Err(err) => {
            // 连库根都规范化不了，说明扫描本身已经失败了（调用方会先报错）。
            // 这里没有可发布状态的地方——不猜一个 root 出来。
            eprintln!("后台建索引未启动（库根不可用）：{err}");
            return;
        }
    };

    let files = sorted_files(scan);
    let truncated = scan.truncated;

    // **必须在 `spawn` 之前**：`vault_scan` 返回之后紧接着的任何一次搜索都要
    // 看见「正在建」，否则它会走 `search` 里的自愈路径，变成第二个建索引的人。
    //
    // `total` 先按篇数报，线程拿到库里的旧状态后会立刻改成
    // 「未变 + 要写 + 要删」的准确值。
    publish(
        &root,
        generation,
        IndexState::Building {
            done: 0,
            total: files.len(),
        },
    );

    let thread_root = root.clone();
    let spawned = std::thread::Builder::new()
        .name("ki-index".to_string())
        .spawn(move || run_background(thread_root, generation, files, truncated));

    if let Err(err) = spawned {
        // 只有资源耗尽才会走到这里。搜索仍会在 `synced_at == 0` 时自愈，
        // 所以这不是死路——但要让用户看见"索引没在跑"。
        eprintln!("无法启动索引线程：{err}");
        publish(
            &root,
            generation,
            IndexState::Failed {
                message: format!("无法启动索引线程：{err}"),
            },
        );
    }
}

/// 后台任务的主体。所有失败都变成可见状态，不往上抛（没有人接）。
fn run_background(root: PathBuf, generation: u64, files: Vec<FileRef>, truncated: bool) {
    let started = Instant::now();
    match background_sync(&root, generation, &files, truncated) {
        Ok(()) => {
            // 回填「最近一次索引维护耗时」：后台建库也是索引维护，不记的话
            // `SearchResult::index_ms` 在生产路径上永远是 0。
            add_index_ms(started.elapsed().as_millis() as u64);
            publish(&root, generation, IndexState::Ready);
        }
        // 代际已变：新的一次扫描接管了。**什么都不说**——状态属于新一代，
        // 这里写任何一个值都会覆盖掉正在建的那个库的进度。
        Err(None) => {}
        Err(Some(err)) => {
            eprintln!("后台建索引失败（树已返回，搜索会少结果）：{err}");
            publish(
                &root,
                generation,
                IndexState::Failed {
                    message: err.to_string(),
                },
            );
        }
    }
}

/// 后台同步。`Err(None)` 表示「被新一代取代」，不是失败。
fn background_sync(
    root: &Path,
    generation: u64,
    files: &[FileRef],
    truncated: bool,
) -> Result<(), Option<KiError>> {
    let (mut db, _outcome) = IndexDb::open(root).map_err(Some)?;
    // 体检可能要几百毫秒，这期间用户完全可能已经切库。
    if super::cache::generation() != generation {
        return Err(None);
    }

    let now = now_ms();
    let beacon = Beacon {
        root,
        generation,
        done: 0,
        total: files.len(),
    };
    match sync(&mut db, root, files, truncated, now, Some(beacon)).map_err(Some)? {
        SyncOutcome::Aborted(_) => Err(None),
        SyncOutcome::Done(_) => {
            // 最后一道代际检查：`sync` 的检查与这里之间还有一段（写 `synced_at`
            // 本身）。被取代时不写——否则新库会看到一个"已同步"的时间戳。
            if super::cache::generation() != generation {
                return Err(None);
            }
            db.set_synced_at(now).map_err(Some)?;
            Ok(())
        }
    }
}

/// 一次同步的两种结束方式。
///
/// 分开而不是返回 `bool`：`Aborted` 的**处置**和 `Done` 完全不同——
/// 不写 `synced_at`、不发布就绪、不把部分结果当成完整结果。
enum SyncOutcome {
    Done(SyncStats),
    /// 代际变了（或者测试钩子叫停）：已经提交的批次留在库里，剩下的不写了。
    Aborted(SyncStats),
}

impl SyncOutcome {
    /// `(stats, 是否中途停下)`。
    fn into_parts(self) -> (SyncStats, bool) {
        match self {
            SyncOutcome::Done(stats) => (stats, false),
            SyncOutcome::Aborted(stats) => (stats, true),
        }
    }
}

/// 一次后台同步的进度信标。
///
/// 它同时承担两件事：**代际检查**（每批写库之前）与**进度发布**（每批之后）。
/// 两者都只碰 `cache` 里的一个原子量与一把几微秒的锁——绝不碰 `CACHE`。
struct Beacon<'a> {
    root: &'a Path,
    generation: u64,
    done: usize,
    total: usize,
}

impl Beacon<'_> {
    /// 我这一代还是当前的吗。
    fn superseded(&self) -> bool {
        generation() != self.generation
    }

    fn publish(&self) {
        publish(
            self.root,
            self.generation,
            IndexState::Building {
                done: self.done,
                total: self.total,
            },
        );
    }

    fn advance(&mut self, processed: usize) {
        self.done += processed;
        self.publish();
    }
}

/// 测试钩子。
///
/// **无条件编译**（与 `autosave::reset_for_test` 同理）：集成测试链接的是
/// 普通 lib，看不到只在 `#[cfg(test)]` 下存在的符号。
///
/// 它存在的唯一理由是「建到一半」这个状态需要**确定**的中点。靠夹具规模去
/// 卡时间窗在 Linux 上会假绿（1000 篇建库只有几十毫秒），在慢机器上会假红。
#[doc(hidden)]
pub mod hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 每批写库之后做什么：参数是**已提交的批数**（从 1 开始），
    /// 返回 `false` 表示「现在停下」，效果等同于一次崩溃（`synced_at` 不写）。
    pub type AfterBatch = Box<dyn FnMut(usize) -> bool + Send>;

    /// 已安装的钩子。`None` 是生产环境的常态。
    pub static AFTER_BATCH: Mutex<Option<AfterBatch>> = Mutex::new(None);

    /// 定位线程数的覆盖。`0` = 按机器自动（`available_parallelism` 钳到 8）。
    ///
    /// 只给测试与基准用：验收要求「同一份 pool 在 1/2/4/8 线程下 `hits` 逐元素
    /// 相同」，而线程数若只由 `available_parallelism()` 决定，这条用例在 8 核
    /// 机器上永远只测得到一种取值。
    ///
    /// 它**不是**用户配置：不持久化、不走 IPC、不进 `bindings.ts`。
    pub static LOCATE_THREADS: AtomicUsize = AtomicUsize::new(0);

    /// 写 [`LOCATE_THREADS`]。`0` 恢复自动。
    pub fn set_locate_threads(threads: usize) {
        LOCATE_THREADS.store(threads, Ordering::Relaxed);
    }

    /// 当前配置下，这个长度的 pool 会用几个线程。
    ///
    /// 对拍用例必须能断言「覆盖真的生效了」，否则一个没接上的钩子会让
    /// 「1/2/4/8 线程下结果相同」变成一句废话——四次跑的都是同一条串行路径。
    pub fn locate_threads_for_test(pool_len: usize) -> usize {
        super::locate_threads(pool_len)
    }
}

/// 调一次测试钩子。生产环境没有钩子，只多一次空锁（每次同步几十次）。
fn after_batch(batches: usize) -> Option<bool> {
    let mut hook = hooks::AFTER_BATCH
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    hook.as_mut().map(|hook| hook(batches))
}

/// 扫描树 → 同步要的扁平文件表，按路径排序。
///
/// 扫描器的顺序是「目录优先 + 名字不区分大小写」；同步只要一个确定性顺序，
/// 这样两批之间的分界在同样的输入下总是一样的（便于复现问题）。
fn sorted_files(scan: &ScanResult) -> Vec<FileRef> {
    let mut files = flatten_files(&scan.tree);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
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

fn sync(
    db: &mut IndexDb,
    root: &Path,
    files: &[FileRef],
    truncated: bool,
    now: i64,
    mut beacon: Option<Beacon<'_>>,
) -> Result<SyncOutcome, KiError> {
    let started = Instant::now();
    let mut stats = SyncStats::default();

    // 库根只规范化一次，父目录链按**去重后的父目录集合**各查一次。
    //
    // 句柄只活在这一个函数里，这是刻意的：它内部的父目录缓存看不见
    // 「父目录在这期间变成了软链」。让它跨过 `sync` 的边界，就会把一个
    // 批处理内的可接受窗口变成进程级的不可见状态——那正是 B-09 的坑。
    // 详见 `ki_vault::ResolvedVault` 的「缓存只能是单批次的」。
    let vault = ResolvedVault::open(root)?;

    let state = db.load_state()?;
    let known: HashMap<&str, &NoteState> = state.iter().map(|s| (s.path.as_str(), s)).collect();

    let mut jobs: Vec<Job> = Vec::new();
    for file in files {
        match known.get(file.path.as_str()) {
            // `size` 与 `mtime` 都没变 → 不读内容。**跳过的也不重试**：
            // 一个 3MB 的文件在它被改小之前，重试只是白白读 3MB。
            Some(prev) if prev.size == file.size && prev.mtime == file.mtime => {
                stats.unchanged += 1;
            }
            _ => jobs.push(Job::Write(file.clone())),
        }
    }

    // `truncated` 时**绝不删**：扫描在文件数上限处截断，缺失的路径
    // 只代表"没扫到"，不代表"文件没了"。拿截断结果做删除会把上限之后的
    // 全部笔记从索引里抹掉——而且下一次搜索会静默地少掉一大批结果。
    if !truncated {
        let live: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
        for prev in &state {
            if !live.contains(prev.path.as_str()) {
                jobs.push(Job::Remove(prev.path.clone()));
            }
        }
    }

    if let Some(beacon) = beacon.as_mut() {
        // 到这里才知道准确的工作量：未变的篇数 + 要写/要删的条数。
        // `spawn_after_scan` 之前报的是"篇数"，两者只差被删掉的那几篇。
        beacon.done = stats.unchanged;
        beacon.total = stats.unchanged + jobs.len();
        beacon.publish();
    }

    let mut batches = 0usize;
    for chunk in jobs.chunks(BATCH_SIZE) {
        // 代际检查放在 `prepare` **之前**：读盘是这一段里最贵的动作
        // （10k 库单批约 250ms），过期了就不该再读。
        if beacon.as_ref().is_some_and(Beacon::superseded) {
            return Ok(SyncOutcome::Aborted(stats));
        }

        // 阶段一：读盘与分词（事务外）。这里是最慢的一段，不能占着写锁。
        let prepared = prepare(chunk, &vault, &known, &mut stats)?;

        // 再对一次：`prepare` 期间用户可能已经切库，否则这一批会写进一个
        // 已经不想要的库。
        if beacon.as_ref().is_some_and(Beacon::superseded) {
            return Ok(SyncOutcome::Aborted(stats));
        }

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

        batches += 1;
        if let Some(beacon) = beacon.as_mut() {
            beacon.advance(chunk.len());
        }
        if after_batch(batches) == Some(false) {
            return Ok(SyncOutcome::Aborted(stats));
        }
    }

    stats.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(SyncOutcome::Done(stats))
}

/// 把一批任务变成"只等写库"的形态。
///
/// 单篇失败**不连坐**：非 UTF-8、超大、IO 失败的条目只记成 `skipped` 并计数。
/// 一条坏文件让整次同步报错，等于用户放了一个 GBK 文件就再也建不了索引。
///
/// 收的是**句柄**而不是 `root`：路径闸的开销按「库根一次 + 每个父目录一次」
/// 摊，而不是按「每篇两次 canonicalize」。这是 B-27 第 ① 步的全部内容。
fn prepare(
    chunk: &[Job],
    vault: &ResolvedVault,
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

        match read_note_in(vault, &file.path) {
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

    // 索引状态与本次读的快照取在同一时点。
    //
    // 顺序很重要：**先读状态，再查库**。反过来的话，后台在查询期间刚好写完，
    // 我们会拿着"用旧快照算出的部分结果"配一个 `Ready`——那是在骗人。
    // 现在这个顺序只可能反过来偏保守（说"在建"而其实已建完），无害。
    let mut index_state = state_for(&root);

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
        if matches!(index_state, IndexState::Building { .. }) {
            // **让路**：后台任务正在建同一个库。两个线程同时写，轻则重复读
            // 一遍全库，重则互相等锁。这里直接返回部分结果 + `Building`——
            // 那正是本次改动要的语义：不全，但可见。
            //
            // 这也是「自愈路径不能变成第二个建索引的人」那条约束的全部实现。
        } else {
            let heal_generation = generation();
            let started = Instant::now();
            let scan = scan_vault(&root, &ScanOptions::default())?;
            let files = sorted_files(&scan);
            let now = now_ms();
            let (_, aborted) = sync(db, &root, &files, scan.truncated, now, None)?.into_parts();
            if aborted {
                // 跑到一半被新一代取代：状态归新一代管，这里不写。
                index_state = state_for(&root);
            } else {
                db.set_synced_at(now)?;
                publish(&root, heal_generation, IndexState::Ready);
                index_state = IndexState::Ready;
            }
            open_ms += started.elapsed().as_millis() as u64;
        }
    }

    if let Some(path) = refresh {
        // 刷新失败不该让搜索失败：用户要的是结果，不是错误。
        let _ = refresh_path(db, &root, path);
    }

    let counts = db.counts()?;
    let qplan = plan(query);
    // 拆词只做一次：每篇定位都要用它，逐篇 `fold_ascii` 会白分配一遍。
    let needles = QueryNeedles::new(query);

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
            // 全扫读过每一篇，`total` 恒为准确值——除非某篇读不到，
            // 那时由 `locate_all` 自己把它降级成下界。
            locate_all(&root, &pool, &needles, counts, false)
        }
        Mode::Ascii | Mode::Cjk => indexed_search(db, &root, &needles, &qplan, counts)?,
    };

    result.index_ms = pending_ms + open_ms;
    result.builds = builds;
    result.index_state = index_state;
    Ok(result)
}

/// 走 FTS 通道：候选 → RRF 融合 → 定位。
///
/// # 候选池的大小是算出来的，不是拍出来的
///
/// 池子只有一个作用：**让 `total` 数得准**。数得准的前提是「候选全在手里」，
/// 而候选有多少只有索引知道。所以两条通道都取 `POOL_EXACT_MAX + 1` 条：
/// 融合之后不超过 `POOL_EXACT_MAX`，就说明**没有任何一条通道取满**，
/// 池子 = 全部候选，`total` 是准确值；否则池子按 `POOL_SCAN_MAX` 截断，
/// `total` 降级为下界，`truncated` 为真。
///
/// 旧实现把「通道取满 200 条」当成饱和判据，于是「命中 1000 篇」与
/// 「命中 200 篇」在池子里长得一模一样，`total` 报 200 而全库是 1000
/// ——UI 说「至少 200 条」，用户以为只有 200 条。
fn indexed_search(
    db: &IndexDb,
    root: &Path,
    query: &QueryNeedles,
    qplan: &Plan,
    counts: Counts,
) -> Result<SearchResult, KiError> {
    let limit = POOL_EXACT_MAX + 1;
    let channel = match qplan.mode {
        Mode::Ascii => db.match_ascii(&qplan.expr, limit)?,
        Mode::Cjk => db.match_zh(&qplan.expr, limit)?,
        _ => unreachable!("indexed_search 只处理 Ascii / Cjk"),
    };

    // 文件名通道与 FTS 通道互不替代：`name_hit` 是排序契约的第一键，
    // 一篇文件名命中但正文没命中的笔记必须进池，否则「按文件名搜」直接失效。
    // 它和 FTS 通道用同一个预算：判据是「池子完整」，不是「每条通道各自完整」。
    let needle = query.whole();
    let names = if needle.is_empty() {
        Vec::new()
    } else {
        db.match_name(needle, limit)?
    };

    let fused = fuse(&[
        channel.iter().map(|c| c.id).collect(),
        names.iter().map(|c| c.id).collect(),
    ]);

    let mut paths: HashMap<i64, &str> = HashMap::with_capacity(channel.len() + names.len());
    for candidate in channel.iter().chain(names.iter()) {
        paths.insert(candidate.id, candidate.path.as_str());
    }

    // 池子完整 ⇔ 融合后的候选不超过预算。取满任意一条通道都会让融合结果
    // 至少等于那个上限，所以这一个判断同时覆盖两条通道。
    let (take, complete) = pool_plan(fused.len());
    let pool: Vec<String> = fused
        .iter()
        .take(take)
        .filter_map(|hit| paths.get(&hit.id).map(|path| (*path).to_string()))
        .collect();

    Ok(locate_all(root, &pool, query, counts, !complete))
}

/// 候选池的取法：完整就全取，超预算就按扫描上限截断。
///
/// 抽成纯函数是为了让 1200/1201 这条边界能被一条**不建索引**的用例钉住——
/// 边界用例若只能靠 1201 篇夹具来测，跑一次十几秒，最后一定会被人删掉。
fn pool_plan(candidates: usize) -> (usize, bool) {
    if candidates <= POOL_EXACT_MAX {
        (candidates, true)
    } else {
        (POOL_SCAN_MAX, false)
    }
}

/// 定位阶段的线程数上限。
///
/// # 为什么是 8
///
/// 并行读盘的收益在 8 线程已经基本吃完。本机探针（1000 篇 pool、release、
/// 杀软实时保护开着、同一进程、5 次取中位数）得到 1/2/4/8/16 线程 =
/// 1.00 / 2.02 / 2.97 / **3.27** / 3.97×：8→16 只多 0.7×，代价是双倍的
/// 在途内存（每线程 ≤ 4 MB）与一倍的线程。ADR-007 的纯 open 微基准
/// 同样是「8→16 已饱和」（2427 / 2259 / 2154 ms）。
const MAX_LOCATE_THREADS: usize = 8;

/// pool 短到这个数以内就串行。
///
/// 开线程的固定开销与「读 1–2 篇」是同一个量级。`00042` 那种单候选查询
/// 改前是 5.32 ms，是搜索里最快的一条路径——并行化不许把它变慢。
const MIN_PARALLEL_POOL: usize = 3;

/// 一个 worker 的局部结果。
///
/// 每个线程攒自己的一份，最后**按 worker 序号**合并。跨线程只做 `usize` 的
/// `+=` 与 `bool` 的 `|=`，不引入 `Mutex`，也不依赖「谁先完成」——顺序由
/// [`finish`] 的全序排序决定。
#[derive(Default)]
struct LocatePart {
    hits: Vec<SearchHit>,
    lines: usize,
    failed: bool,
}

/// 本次定位用几个线程。
///
/// 判据只有两条：pool 太短就串行（省建线程），否则按机器核数钳到
/// [`MAX_LOCATE_THREADS`]。**不是用户配置**——唯一的覆盖入口是
/// [`hooks::LOCATE_THREADS`]，只给测试与基准。
fn locate_threads(pool_len: usize) -> usize {
    if pool_len < MIN_PARALLEL_POOL {
        return 1;
    }
    let forced = hooks::LOCATE_THREADS.load(Ordering::Relaxed);
    let wanted = if forced > 0 {
        forced
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get())
    };
    wanted.clamp(1, MAX_LOCATE_THREADS).min(pool_len)
}

/// 一篇的定位：读盘 → 构造定位器 → 找命中。`None` 表示这篇读不到。
fn locate_one(root: &Path, path: &str, query: &QueryNeedles) -> Option<Located> {
    let note = read_note(root, path).ok()?;
    Some(NoteLocator::new(note.path, note.content).locate(query, MAX_HITS_PER_FILE))
}

/// 串行定位一整段。1 线程与短 pool 走这条。
fn locate_serial(root: &Path, pool: &[String], query: &QueryNeedles) -> LocatePart {
    let mut part = LocatePart::default();
    for path in pool {
        match locate_one(root, path, query) {
            Some(located) => {
                part.lines += located.lines;
                part.hits.extend(located.hits);
            }
            // 读不到 ⇒ 少了一篇的命中 ⇒ `total` 只能是下界。
            None => part.failed = true,
        }
    }
    part
}

/// 对候选池逐篇定位，产出最终结果。
///
/// 读不到的文件静默跳过：它可能在扫描与搜索之间被删掉了。**不要**因此把
/// 整次搜索变成错误——一篇被删的笔记不该让用户搜不了整个库。但它确实让
/// `total` 少算了一篇的命中，所以结果要降级成「下界」（`truncated = true`）。
///
/// 逐篇的那一步被抽成 [`locate_one`]、整段被抽成 [`locate_serial`]：并行的
/// 版本要复用同一个「读一篇 → 定位 → 攒命中」单元，否则两条路的判定逻辑会
/// 分叉。本提交只做这次抽取，行为逐字节不变。
fn locate_all(
    root: &Path,
    pool: &[String],
    query: &QueryNeedles,
    counts: Counts,
    pool_saturated: bool,
) -> SearchResult {
    let part = locate_serial(root, pool, query);

    // 两条让 `total` 变成下界的路合成一个信号：池子被预算截断，或池里有篇
    // 读不到。
    finish(
        part.hits,
        part.lines,
        counts,
        pool_saturated || part.failed,
    )
}

/// 排序、截断、填诊断字段。
///
/// 排序键与旧引擎**逐字相同**：文件名命中优先 → 路径 → 行号。三者都是确定的，
/// 所以结果与遍历顺序无关（候选池的顺序来自 RRF，但它不影响最终顺序）。
///
/// `truncated` **只表示一件事**：`total` 是下界。它不再是「`hits` 被截到
/// 200 条」的同义词——那件事永远成立（`hits` 就是只返回 200 条），把它也
/// 算进 `truncated` 会让「total 准确」的结果被 UI 说成「至少 N 条」。
/// 契约只有一条：**`total` 不准 ⇔ `truncated` 为真**。
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

    hits.truncate(MAX_HITS);

    SearchResult {
        hits,
        total,
        truncated: pool_saturated,
        indexed_files: counts.ok,
        skipped_files: counts.skipped,
        index_ms: 0,
        builds: 0,
        // 由调用方覆盖：`finish` 看不到缓存，也不该为了一个诊断字段把锁传进来。
        index_state: IndexState::Idle,
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
/// - 进程里还没有连接（比如刚扫描完，索引在后台建）：**自己开一个**。
///   不能"没有连接就什么都不做"——那会让恢复完的笔记永远缺一行，
///   而调用方拿到的是 `Ok`。
/// - 索引还没同步过（`synced_at == 0`）：**照样写这一行**。旧版本在这里
///   直接返回（理由是"下一次搜索会全量自建"），但后台建索引正在跑时那句话
///   不成立：它的扫描结果里没有这篇（文件那时还在回收站），它也不会再扫一次。
///   写进去是安全的——行结构与 `sync` 写的完全一样，之后的全量同步看到
///   size/mtime 相同就判「未变」。
/// - 读不到（刚又被删了、编码坏了、超过 2MB）：**删掉那一行**。超大文件
///   下次扫描会以 `skipped` 的身份回来，不值得为它多一条分支。
/// - 失败由调用方吞掉：恢复已经成功，索引问题不该让用户以为「没恢复」。
pub fn reindex(root: &Path, rel: &str) -> Result<(), KiError> {
    let root = canonical_root(root)?;
    let mut cache = lock();
    ensure_root(&mut cache, &root);

    // 连接没有就自己开一个。
    //
    // `vault_scan` 改成后台建索引之后，它**不再**顺手把连接放进缓存
    // （后台任务用的是它自己的连接）。这里若沿用"没有连接就什么都不做"，
    // 恢复完的笔记会永远缺一行，而调用方拿到的是 `Ok`——静默的少结果，
    // 正是 `reindex` 存在的理由。
    if cache.store.is_none() {
        let (db, _) = IndexDb::open(&root)?;
        cache.store = Some(db);
    }
    let db = cache.store.as_mut().expect("上面刚保证过 store 非空");

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 池子的边界：预算以内全取（`total` 准确），预算之外截到扫描上限（下界）。
    ///
    /// 这条用例存在的唯一理由是「1200 与 1201 差一条」这个判据不值得用
    /// 1201 篇夹具去测，而它又是 `truncated` 会不会漏报的唯一边界。
    #[test]
    fn pool_plan_is_exact_up_to_the_budget() {
        assert_eq!(pool_plan(0), (0, true));
        assert_eq!(pool_plan(1), (1, true));
        assert_eq!(pool_plan(POOL_EXACT_MAX), (POOL_EXACT_MAX, true));
        assert_eq!(pool_plan(POOL_EXACT_MAX + 1), (POOL_SCAN_MAX, false));
        assert_eq!(pool_plan(10_000), (POOL_SCAN_MAX, false));
    }

    /// 下界必须**小于**预算，否则「截断」比「全取」还贵。
    #[test]
    fn scan_cap_is_much_smaller_than_the_exact_budget() {
        assert!(POOL_SCAN_MAX < POOL_EXACT_MAX);
        assert_eq!(POOL_SCAN_MAX, MAX_HITS, "截断时刚好够填满结果列表");
    }
}
