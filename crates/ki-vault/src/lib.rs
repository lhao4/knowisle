//! 知屿 KnowIsle · Vault 文件系统层
//!
//! 本 crate 是"磁盘上的 Markdown 是唯一真相源"（公理 A1）在代码里的落点。
//! 它只做四件事：**扫描**、**路径安全**、**读写**、**全文索引**。
//!
//! 全文索引（[`search`]）刻意是"朴素子串扫描 + 进程内存"，不是搜索引擎：
//! 它读的是同一批文件，遵守同一份忽略名单，因此必须和扫描器住在一起。
//! M3 的 FTS5 索引会搬去 `ki-search`，那时这里只留"给索引喂数据"的接口。
//!
//! 写入遵循公理 A5（`tmp → fsync → rename`），实现集中在 [`write`] 模块。
//! 自动保存的**时机决策**在 [`autosave`]——但只有纯逻辑，线程与状态在 `ki-ipc`。
//! 更高层的东西（历史快照、oplog）不在本 crate，它们属于 M1 之后。

pub mod autosave;
pub mod path;
pub mod scan;
pub mod search;
pub mod write;

pub use autosave::{SaveThrottle, AUTOSAVE_INTERVAL};
pub use path::{resolve_for_write, resolve_in_vault, to_posix};
pub use scan::{read_note, scan_vault, ScanOptions, MAX_SAFE_DEPTH};
pub use search::{SearchIndex, MAX_HITS, MAX_HITS_PER_FILE, MAX_INDEX_BYTES};
pub use write::{write_note, write_note_checked};
