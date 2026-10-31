//! 知屿 KnowIsle · 领域层
//!
//! 本 crate 是架构公理 A2 的物理体现：**不依赖 tauri、不依赖任何 IO 库**。
//! 它只描述"数据长什么样"和"什么状态是非法的"，不描述"怎么读写"。
//!
//! 读写能力全部在 `ki-vault`（文件系统）、`ki-store`（SQLite）等基础设施层，
//! 它们依赖本 crate，而不是反过来。
//!
//! # 关于 `specta::Type`
//!
//! 领域类型同时 derive `Serialize` 与 `Type`：前者定义"跨 IPC 时的 JSON 形状"，
//! 后者把这个形状单向投影成 TypeScript。两者必须长在一起——分开写就是两套契约，
//! 而两套契约迟早会不一致。`specta` 是纯类型反射库，不带 IO、不带 tauri，
//! 因此没有违反 A2。

pub mod encoding;
pub mod error;
pub mod ignore;
pub mod info;
pub mod node;
pub mod search;
pub mod skip;
pub mod trash;

pub use encoding::{EncodingEntry, EncodingStats, TranscodeBatch, TranscodeFailure, TranscodeProgress};
pub use error::{IoKind, KiError};
pub use ignore::{IgnoreConfigError, IgnoreRules};
pub use info::VaultInfo;
pub use node::{AppInfo, FileNode, NodeKind, NoteContent, SaveResult, SaveState, ScanResult};
pub use search::{IndexState, SearchHit, SearchResult};
pub use skip::{IndexSkipEntry, IndexSkipReason, IndexSkipStats, ScanSkipEntry, ScanSkipReason};
pub use trash::{Fallback, PurgeResult, RestoreResult, TrashCursor, TrashEntry, TrashPage};

/// 本应用的元数据目录（Vault 相对路径的第一级）。
///
/// 四处必须引用同一个常量，否则会各自漂移：
/// - `ki-vault::scan` 的忽略名单（扫描器「看不见它」）
/// - `ki-vault::path` 的破坏性操作闸（改名/删除不许碰它）
/// - `ki-store` 的索引库位置（`<root>/.knowisle/index.sqlite`）
/// - `ki-vault::ignore` 的配置文件位置（`<root>/.knowisle/config.toml`）
///
/// 最后一条与前三条例外：前三样都是**派生数据**（可整库重建），配置文件
/// 是**用户输入**。所以「重建」只许删 `index.sqlite`，不许删这个目录——
/// 见 ADR-014。
///
/// 它放在领域层而不是 `ki-vault`：索引库也要用它，而 `ki-store` 不该为了一个
/// 字符串常量去依赖文件系统层。
pub const METADATA_DIR: &str = ".knowisle";
