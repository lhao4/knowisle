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

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use ki_core::KiError;
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

pub(crate) fn lock() -> MutexGuard<'static, Cache> {
    // 建索引里没有任何会 panic 的 `unwrap`，但毒化不该让搜索永久瘫痪：
    // 拿回内部值继续用，最坏情况是重建一次。
    CACHE.lock().unwrap_or_else(PoisonError::into_inner)
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
/// 索引库的**文件**不动——它马上会被 `sync_after_scan` 更新，删掉只会让
/// 紧接着的同步多做一次体检与全量重建。
pub(crate) fn invalidate(cache: &mut Cache, root: &Path) {
    if cache.root.as_deref() == Some(root) {
        cache.legacy = None;
        cache.store = None;
    }
}
