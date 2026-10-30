//! 进程级缓存：当前 Vault、旧索引、索引库连接。
//!
//! # 锁为什么覆盖整个"建索引 / 打开库"过程
//!
//! 连打 3 个字符就是 3 次搜索。若只在"读写缓存"时加锁、建索引放在锁外，
//! 三个请求会同时发现"没有缓存"，于是**同时建三份索引**：IO 与内存都翻三倍，
//! 而且最后谁写回缓存全看调度。这里让后来者直接排队，拿到锁时缓存已经有了，
//! 于是只建一份、只读一次盘。
//!
//! 代价是"一个慢查询挡住另一个查询"。在 1000 篇的规模上，这个代价是几毫秒；
//! 而三份索引同时构建的代价是三次全库读盘。前者是排队，后者是雪崩。
//!
//! 索引库连接同理：`IndexDb::open` 要做体检（`quick_check` 扫全库页结构），
//! 10k 篇的库上不是免费操作，不能被并发请求各做一遍。
//!
//! # 后台索引**不碰**这把锁
//!
//! 建索引还有一个入口：`vault_scan` 之后的后台线程。它若也拿 `CACHE`，
//! 就会持着这把锁跑 10–100 秒，于是 `vault_search` 全程排队——那等于把
//! 「首屏阻塞」换成「搜索阻塞」，而后者更难被发现（用户只看到"搜不出来"）。
//!
//! 所以后台线程**自己** `IndexDb::open` 一个连接（WAL 下读写可并发），
//! 只往下面那两个静态量里写**轻量状态**：一个 `AtomicU64` 代际，
//! 一个装进 `Mutex` 的进度。那把锁只在这几微秒里被持有。
//!
//! 两条路径共用同一个库文件，靠 SQLite 的写锁串行化。**不要**为了"省一次
//! `quick_check`"让后台线程复用 `CACHE.store`：那会把上面那句「不碰」变成谎言。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ki_core::{IndexState, KiError};
use ki_store::IndexDb;
use ki_vault::SearchIndex;

pub(crate) struct Cache {
    /// 当前缓存的 Vault（已规范化）。`None` 表示还没建过。
    pub(crate) root: Option<PathBuf>,
    /// M2 的旧引擎。`None` 表示还没建，或者刚被 `vault_scan` 丢掉。
    pub(crate) legacy: Option<SearchIndex>,
    /// 索引库连接。切库时释放，同库内跨扫描复用。
    pub(crate) store: Option<IndexDb>,
    /// 当前 root 建过几次索引。切 root 时归零，`invalidate` 之后继续累加。
    ///
    /// 它是诊断字段：外部唯一能证明"并发时只建了一份"的证据。
    pub(crate) builds: u32,
    /// 上一次索引维护（打开/重建 + 扫描同步 + refresh）的耗时，
    /// 由下一次查询取走并清零。见 `search::service` 的说明。
    pub(crate) pending_index_ms: u64,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    root: None,
    legacy: None,
    store: None,
    builds: 0,
    pending_index_ms: 0,
});

/// 后台索引的代际。**每次 `vault_scan` +1。**
///
/// 后台任务启动时记下自己那一代，**每一批写库之前**对一次：不等就立刻退出
/// 并释放连接。这是「切库时旧任务必须立刻失效」的唯一机制——没有它，A 库的
/// 后台线程会继续往 A 的索引库里写，而用户已经切到 B 了（更糟：若两库共用
/// 一个路径，写进去的就是错的行）。
///
/// # 为什么是 `u64` 而不是 `bool`
///
/// `bool` 表达不了「A 被 B 取代，B 又被 C 取代」：A 在 B 启动之后仍会认为
/// 自己是当前（`running == true`）。代际只做等值比较，谁都可以被后来者取代。
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 后台索引的可见状态。
///
/// 锁只覆盖「读/写这几个字段」，绝不覆盖建索引过程。代际一变它立刻作废——
/// 不需要谁去清理，也就不存在「切库后旧状态一直挂着，于是搜索永远让路」。
static PROGRESS: Mutex<Progress> = Mutex::new(Progress {
    generation: 0,
    root: None,
    state: IndexState::Idle,
});

struct Progress {
    /// 这份状态属于哪一代。与当前代际不等即视为 [`IndexState::Idle`]。
    generation: u64,
    root: Option<PathBuf>,
    state: IndexState,
}

pub(crate) fn lock() -> MutexGuard<'static, Cache> {
    // 建索引里没有任何会 panic 的 `unwrap`，但毒化不该让搜索永久瘫痪：
    // 拿回内部值继续用，最坏情况是重建一次。
    CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 开新一代，返回新的代际号。
///
/// 只有 `vault_scan` 调它：每一次「重新看一眼磁盘」都是一次新的意图，
/// 之前所有在飞的建索引任务都该作废。
pub(crate) fn bump_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// 同一时刻只允许一个同步在跑。**进程级，不按 root 分**。
///
/// # 为什么代际不够
///
/// 两次 `vault_scan` 并发（React StrictMode 双挂载，或用户连点两次「选择
/// 文件夹」）会各自 `spawn_after_scan`，于是两个线程同时 `IndexDb::open`
/// 同一个 WAL 库。代际只保证「旧的那个不发布状态」，**不保证它不写库**——
/// 它在下一个批次边界才退出，而那个边界可能是一批 250 ms 之后。
///
/// 实测（8 轮并发扫描、每轮两个线程，第 3 轮复现）：另一个线程的写会拿到
/// `SQLITE_READONLY`（`attempt to write a readonly database`），整次同步作废、
/// 索引停在上一版。用户看到的现象是「删掉的笔记还留在跳过清单里」，
/// 而重跑一次又好了——最难查的那一类。
///
/// 串行化的代价是第二次同步要等第一次退出。第一次在下一个批次边界就发现
/// 自己被取代并退出（10k 篇约 250 ms），换的是「任何时刻只有一个写者」
/// 这条不变量。**不要**为了省这 250 ms 把它改成按 root 加锁：索引库是
/// 一个文件，写者只能有一个。
pub(crate) fn sync_gate() -> MutexGuard<'static, ()> {
    static GATE: Mutex<()> = Mutex::new(());
    GATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 发布后台索引的进度或终态。
///
/// 代际已变则丢弃：旧任务不许覆盖新库的状态。检查与写入之间仍有窗口，
/// 但 `Progress::generation` 会让那次写入在读取侧立刻失效——所以这里
/// 不需要把 `GENERATION` 也塞进锁里。
pub(crate) fn publish(root: &Path, generation: u64, state: IndexState) {
    if generation != self::generation() {
        return;
    }
    let mut progress = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    progress.generation = generation;
    progress.root = Some(root.to_path_buf());
    progress.state = state;
}

/// 读某个库的可见索引状态。
///
/// 代际或 root 对不上都返回 [`IndexState::Idle`]：那份状态属于上一代或
/// 另一个库，对当前这次搜索没有意义。
pub(crate) fn state_for(root: &Path) -> IndexState {
    let current = generation();
    let progress = PROGRESS.lock().unwrap_or_else(PoisonError::into_inner);
    if progress.generation == current && progress.root.as_deref() == Some(root) {
        progress.state.clone()
    } else {
        IndexState::Idle
    }
}

/// 把后台建索引的耗时记进 `pending_index_ms`，由下一次搜索取走并清零。
///
/// 这**不是**「后台任务碰 `CACHE`」的反例：它只加一个 `u64`，锁的持有时间是
/// 纳秒级；「不碰」指的是不持着那把锁跑 100 秒的建索引过程。
///
/// 不这么做的话，`SearchResult::index_ms` 在生产路径上恒为 0——一个永远是 0
/// 的诊断字段比没有更糟：看的人会以为"索引维护不花时间"。
pub(crate) fn add_index_ms(ms: u64) {
    let mut cache = lock();
    cache.pending_index_ms += ms;
}

/// 把 root 规范化成与 `scan_vault` 相同的形态。
///
/// 不规范化的话，`/tmp/vault` 与 `/tmp/vault/` 会被当成两个库，
/// 每次切换都要重建索引——表现为"怎么每次搜索都慢"。
pub(crate) fn canonical_root(root: &Path) -> Result<PathBuf, KiError> {
    dunce::canonicalize(root).map_err(|err| KiError::io(root.to_string_lossy(), &err))
}

/// 缓存指向 `root`；切库时把上一个 Vault 的所有派生状态丢掉。
///
/// 旧索引与旧连接都没有复用价值：没有 watcher（B-09），跨库的索引一定是错的。
pub(crate) fn ensure_root(cache: &mut Cache, root: &Path) {
    if cache.root.as_deref() == Some(root) {
        return;
    }
    cache.root = Some(root.to_path_buf());
    cache.legacy = None;
    cache.store = None;
    cache.builds = 0;
    cache.pending_index_ms = 0;
}

/// 丢掉某个 Vault 的进程内派生状态（旧索引 + 索引库连接）。
///
/// `vault_scan` 调用它：用户点「选择文件夹」就是明确要求"重新看一眼磁盘"，
/// 此时还留着旧索引，等于把"外部改动要重新扫描才可见"这句话变成谎言。
///
/// **`builds` 不清零**：它是「本进程为这个 Vault 建过几次索引」的累计值，
/// 重新扫描正是要让它 +1。切库才归零（见 [`ensure_root`]）。
///
/// 索引库的**文件**不动——它马上会被 `spawn_after_scan` 的后台任务更新，
/// 删掉只会让紧接着的同步多做一次体检与全量重建。
///
/// 连接**必须**丢掉：后台任务自己 `IndexDb::open`，而它有可能判定"整库重建"
/// 并删掉索引文件。留着旧连接的话，它指向的是那个已经被 unlink 的 inode
/// （Windows 上则是让删除失败），两条路都不对。
pub(crate) fn invalidate(cache: &mut Cache, root: &Path) {
    if cache.root.as_deref() == Some(root) {
        cache.legacy = None;
        cache.store = None;
    }
}
