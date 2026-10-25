//! 检索的进程级编排。
//!
//! # 三层分工
//!
//! | 模块 | 职责 | 有没有状态 |
//! |---|---|---|
//! | [`legacy`] | M2 的内存索引（旧引擎），整库读进内存逐篇字面匹配 | 无 |
//! | [`service`] | 增量同步（diff → 读盘 → 分批写库）与 FTS 查询编排 | 无 |
//! | `cache` | 进程级缓存：当前 Vault、旧索引、索引库连接、诊断计数 | **有** |
//!
//! 纯逻辑在 `ki-vault`（定位器、扫描）、`ki-search`（分词、计划、RRF）、
//! `ki-store`（SQL），这里只负责"谁在同时用"与"东西活多久"——
//! 与 autosave 的分工完全一样。
//!
//! # 为什么只有一个 Vault 的缓存
//!
//! 同一时刻只可能打开一个 Vault。缓存两个 root 的唯一效果是切库时内存翻倍，
//! 而切回来仍然要重建——没有 watcher（B-09），旧索引不可信。
//!
//! # 两个引擎共存
//!
//! [`ENGINE`] 决定 [`query`] 走哪条路。`Memory` 是 M2 的旧实现，`Fts` 是
//! 新的 FTS5 路径。两者**共用同一个定位器**（`ki_vault::NoteLocator`），
//! 所以它们的检索精度天然一致，差异只可能来自候选集——这正是
//! `tests/search_parity.rs` 能作为硬门槛的原因。
//!
//! 旧引擎计划保留一个 release 再清理：它是回滚路径，也是对拍的参照实现。

mod cache;
pub mod legacy;
pub mod service;

pub use service::{sync_after_scan, SyncReport};

use std::path::Path;

use ki_core::{KiError, SearchResult};

/// 丢掉某个 Vault 的进程内派生状态（旧索引 + 索引库连接）。
///
/// `vault_scan` 调用它：用户点「选择文件夹」就是明确要求"重新看一眼磁盘"，
/// 此时还留着旧索引，等于把"外部改动要重新扫描才可见"这句话变成谎言。
///
/// 它与引擎无关——两个引擎共用同一份缓存，所以作废也必须是同一个动作。
pub fn invalidate(root: &Path) {
    let Ok(root) = dunce::canonicalize(root) else {
        return;
    };
    let mut cache = cache::lock();
    cache::invalidate(&mut cache, &root);
}

/// 检索引擎。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// M2 的内存索引：整库读进内存，逐篇字面匹配。
    Memory,
    /// FTS5 索引：SQLite 选候选，定位器出结果。
    Fts,
}

/// 当前生效的引擎。
///
/// 换引擎改这一行即可，不需要动前端、数据库或命令签名。对拍测试直接调
/// [`legacy::query`] 与 [`service::search`]，不经过这里——否则它就成了
/// 「用同一份代码对比自己」。
///
/// # 为什么现在指向 `Fts`
///
/// 对拍（`tests/search_parity.rs`）已经证明两条路径逐字一致，包括
/// 「旧引擎有、新引擎没有」这种最难查的方向。翻开关之前它是 `Memory`，
/// 翻过来之后旧引擎仍然保留一个 release：它是回滚路径，
/// 也是下一次改检索时的参照实现。
pub const ENGINE: Engine = Engine::Fts;

/// 查询全文索引。这是 `vault_search` 命令唯一会调的函数。
pub fn query(root: &Path, query: &str, refresh: Option<&str>) -> Result<SearchResult, KiError> {
    match ENGINE {
        Engine::Memory => legacy::query(root, query, refresh),
        Engine::Fts => service::search(root, query, refresh),
    }
}
