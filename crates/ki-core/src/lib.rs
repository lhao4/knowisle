//! 知屿 KnowIsle · 领域层
//!
//! 本 crate 是架构公理 A2 的物理体现：**不依赖 tauri、不依赖任何 IO 库**。
//! 它只描述"数据长什么样"和"什么状态是非法的"，不描述"怎么读写"。
//!
//! 读写能力全部在 `ki-vault`（文件系统）、`ki-store`（SQLite）等基础设施层，
//! 它们依赖本 crate，而不是反过来。

pub mod error;
pub mod node;

pub use error::KiError;
pub use node::{FileNode, NodeKind, NoteContent, ScanResult};
