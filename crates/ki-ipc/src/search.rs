//! 全文索引的进程级缓存。
//!
//! # 职责边界
//!
//! [`ki_vault::SearchIndex`] 是纯逻辑：给一个 root，建一份索引，查它。
//! 它不知道"谁在同时查"、也不知道"这份索引活了多久"。那些是**进程状态**，
//! 属于本模块——与 autosave 的分工完全一样（纯逻辑在 ki-vault，线程与状态在 ki-ipc）。
//!
//! # 只缓存一个 root
//!
//! 同一时刻只可能打开一个 Vault。缓存两个 root 的唯一效果是切库时内存翻倍，
//! 而切回来仍然要重新建——没有 watcher（B-09），旧索引不可信。
//!
//! # 锁为什么覆盖整个建索引过程
//!
//! 连打 3 个字符就是 3 次搜索。若只在"读写缓存"时加锁、建索引放在锁外，
//! 三个请求会同时发现"没有缓存"，于是**同时建三份索引**：IO 与内存都翻三倍，
//! 而且最后谁写回缓存全看调度。这里让后来者直接排队，拿到锁时缓存已经有了，
//! 于是只建一份、只读一次盘。
//!
//! 代价是"一个慢查询挡住另一个查询"。在 1000 篇的规模上，这个代价是几毫秒；
//! 而三份索引同时构建的代价是三次全库读盘。前者是排队，后者是雪崩。
//!
//! # 为什么刷新当前笔记放在这里而不是索引里
//!
//! `refresh` 需要 root，而 root 只有缓存知道（`SearchIndex` 内部持有的是
//! 规范化之后的路径）。它仍然是索引的方法，本模块只负责把参数递进去。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use ki_core::{KiError, SearchResult};
use ki_vault::SearchIndex;

struct Cache {
    /// 当前缓存的 Vault（已规范化）。`None` 表示还没建过。
    root: Option<PathBuf>,
    index: Option<SearchIndex>,
    /// 当前 root 建过几次索引。切 root 时归零，`invalidate` 之后继续累加。
    ///
    /// 它是诊断字段：外部唯一能证明"并发时只建了一份"的证据。
    builds: u32,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    root: None,
    index: None,
    builds: 0,
});

fn lock() -> std::sync::MutexGuard<'static, Cache> {
    // 建索引里没有任何会 panic 的 `unwrap`，但毒化不该让搜索永久瘫痪：
    // 拿回内部值继续用，最坏情况是重建一次。
    CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 查询全文索引，必要时先建。
pub fn query(root: &Path, query: &str, refresh: Option<&str>) -> Result<SearchResult, KiError> {
    let root =
        dunce::canonicalize(root).map_err(|err| KiError::io(root.to_string_lossy(), &err))?;
    let mut cache = lock();

    if cache.root.as_deref() != Some(root.as_path()) {
        // 换库：旧索引没有任何复用价值（没有 watcher，它已经不可信了）。
        cache.root = Some(root.clone());
        cache.index = None;
        cache.builds = 0;
    }

    let mut built_now = false;
    if cache.index.is_none() {
        // 锁一直持有到这里：后来者在 `lock()` 上排队，醒来时 `index` 已经有值。
        let index = SearchIndex::build(&root)?;
        cache.index = Some(index);
        cache.builds += 1;
        built_now = true;
    }

    let builds = cache.builds;
    let index = cache.index.as_mut().expect("上面刚保证过 index 非空");
    let mut result = index.search(query, refresh);
    result.index_ms = if built_now { index.built_ms() } else { 0 };
    result.builds = builds;
    Ok(result)
}

/// 丢掉某个 Vault 的缓存索引。
///
/// `vault_scan` 调用它：用户点「选择文件夹」就是明确要求"重新看一眼磁盘"，
/// 此时还留着旧索引，等于把"外部改动要重新扫描才可见"这句话变成谎言。
pub fn invalidate(root: &Path) {
    let Ok(root) = dunce::canonicalize(root) else {
        return;
    };
    let mut cache = lock();
    if cache.root.as_deref() == Some(root.as_path()) {
        cache.index = None;
    }
}
