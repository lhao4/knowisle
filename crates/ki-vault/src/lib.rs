//! 知屿 KnowIsle · Vault 文件系统层
//!
//! 本 crate 是"磁盘上的 Markdown 是唯一真相源"（公理 A1）在代码里的落点。
//! 它只做六件事：**扫描**、**路径安全**、**读写**、**新建 / 改名 / 删除**、
//! **回收站**、**全文索引**。
//!
//! 新建、改名与删除（[`fileops`]）刻意与内容写入（[`write`]）分开：前三者只碰
//! 目录项，后者才是原子写要保护的那条路径。文件名规则（[`naming`]）单独成模块，
//! 因为它是**唯一**的闸门——GUI 与将来的 `isle` CLI 共用同一份判定。
//!
//! 回收站（[`trash`]）是删除的另一半：文件只是被搬进 `.knowisle/trash/`，
//! 原路径记在一条独立的清单里。清单**只描述位置**，正文始终在磁盘上——
//! 所以它仍然是可重建的元数据，不违反 A1（见 ADR-008）。
//!
//! 全文索引（[`search`]）刻意是"朴素子串扫描 + 进程内存"，不是搜索引擎：
//! 它读的是同一批文件，遵守同一份忽略名单，因此必须和扫描器住在一起。
//! M3 的 FTS5 索引会搬去 `ki-search`，那时这里只留"给索引喂数据"的接口。
//!
//! 写入遵循公理 A5（`tmp → fsync → rename`），实现集中在 [`write`] 模块。
//! 自动保存的**时机决策**在 [`autosave`]——但只有纯逻辑，线程与状态在 `ki-ipc`。
//! 更高层的东西（历史快照、oplog）不在本 crate，它们属于 M1 之后。

pub mod autosave;
pub mod fileops;
pub mod naming;
pub mod path;
pub mod scan;
pub mod search;
pub mod trash;
pub mod write;

pub use autosave::{SaveThrottle, AUTOSAVE_INTERVAL};
pub use fileops::{create_note, delete_note, rename_note};
pub use naming::normalize_note_name;
pub use path::{resolve_for_write, resolve_in_vault, to_posix};
pub use scan::{flatten_files, read_note, scan_vault, FileRef, ScanOptions, MAX_SAFE_DEPTH};
pub use search::{
    fold_ascii, name_lower_for_path, note_stem, NoteLocator, SearchIndex, MAX_HITS,
    MAX_HITS_PER_FILE, MAX_INDEX_BYTES,
};
pub use trash::{
    list as trash_list, purge as trash_purge, restore as trash_restore, MANIFEST_DIR, TRASH_DIR,
};
pub use write::{write_note, write_note_checked};
