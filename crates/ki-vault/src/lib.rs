//! 知屿 KnowIsle · Vault 文件系统层
//!
//! 本 crate 是"磁盘上的 Markdown 是唯一真相源"（公理 A1）在代码里的落点。
//! 它只做三件事：**扫描**、**路径安全**、**读取**。
//!
//! 写入（原子写、fsync、历史快照）属于 M1，此处刻意不实现——
//! M0 阶段任何写操作都不存在，也就不存在丢数据的风险。

pub mod path;
pub mod scan;

pub use path::{resolve_in_vault, to_posix};
pub use scan::{read_note, scan_vault, ScanOptions};
