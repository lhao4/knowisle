//! M2 的旧引擎：整库读进内存，逐篇字面匹配。
//!
//! # 它为什么还在
//!
//! 1. **新旧对拍**要有一个独立的参照实现。用同一份代码对比自己是没有意义的。
//! 2. **回滚**：`ki-ipc::search` 里把引擎开关改回 `Memory` 就退回这里，
//!    不需要回滚数据库、不需要改前端。
//!
//! 计划是保留一个 release 再清理。
//!
//! # 它的代价写在明处
//!
//! - 冷启动要读一遍全库（1000 篇实测 100–200ms），首次搜索慢；
//! - 内存约为库内正文的**两倍**（原文 + ASCII 小写副本）；
//! - 外部改动在重新扫描之前搜不到（B-09 的既有缺口，不是本模块引入的）。

use std::path::Path;

use ki_core::{KiError, SearchResult};
use ki_vault::SearchIndex;

use super::cache::{canonical_root, ensure_root, lock};

/// 查询全文索引，必要时先建。
pub fn query(root: &Path, query: &str, refresh: Option<&str>) -> Result<SearchResult, KiError> {
    let root = canonical_root(root)?;
    let mut cache = lock();
    ensure_root(&mut cache, &root);

    let mut built_now = false;
    if cache.legacy.is_none() {
        // 锁一直持有到这里：后来者在 `lock()` 上排队，醒来时 `legacy` 已经有值。
        let index = SearchIndex::build(&root)?;
        cache.legacy = Some(index);
        cache.builds += 1;
        built_now = true;
    }

    let builds = cache.builds;
    let index = cache.legacy.as_mut().expect("上面刚保证过 legacy 非空");
    let mut result = index.search(query, refresh);
    result.index_ms = if built_now { index.built_ms() } else { 0 };
    result.builds = builds;
    Ok(result)
}

/// 丢掉某个 Vault 的缓存索引。
///
/// `vault_scan` 调用它：用户点「选择文件夹」就是明确要求"重新看一眼磁盘"，
/// 此时还留着旧索引，等于把"外部改动要重新扫描才可见"这句话变成谎言。
///
/// **不动索引库连接**：库是要被 `sync_after_scan` 更新的，关掉它只会让
/// 紧接着的同步多做一次体检。
pub fn invalidate(root: &Path) {
    let Ok(root) = dunce::canonicalize(root) else {
        return;
    };
    let mut cache = lock();
    if cache.root.as_deref() == Some(root.as_path()) {
        cache.legacy = None;
    }
}
