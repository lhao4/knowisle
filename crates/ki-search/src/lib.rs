//! 知屿 KnowIsle · 检索规则
//!
//! 这个 crate 里**没有一行 IO**：不读文件、不连数据库、不看 Vault。它只回答
//! 三个问题——
//!
//! 1. 一段正文该被切成哪些 token（[`bigram`]、[`ascii`]）；注意**查询侧与
//!    索引侧不是同一份 token**——索引侧多一组覆盖 token，让单字查询也能走
//!    前缀通道（见 [`bigram::index_tokens`]）；
//! 2. 一个查询该被翻成什么样的 FTS5 表达式、或者干脆走全扫（[`plan`]）；
//! 3. 多路候选该怎么合并成一条有序列表（[`rrf`]）。
//!
//! # 为什么单独成 crate
//!
//! 这三件事都是纯函数，因此可以用几万组随机输入做 property test，
//! 而**不必链接 SQLite**（否则每个 case 都要建库，测试从毫秒变分钟）。
//! 更实际的理由是：它必须能被 `ki-store`、`ki-ipc`、将来的 `isle` CLI
//! 同时依赖，而不把 `rusqlite` 传染给其中任何一个。
//!
//! # 一条贯穿全模块的分工
//!
//! **索引层只负责「不漏候选」，判定仍由字面定位器做。**
//!
//! 这条分工决定了这里所有的「宁可多不可少」：bigram 只会让候选变多
//! （AND 的是查询的全部 bigram，文档缺一个就落选，所以不会漏；但反过来
//! 文档有这些 bigram 却不含原串时也会落选，那由定位器兜底）。多出来的候选
//! 由 `NoteLocator` 逐行字面判定后丢掉——它才是精度的唯一来源。

pub mod ascii;
pub mod bigram;
pub mod plan;
pub mod rrf;

pub use ascii::project;
pub use bigram::{index_tokens, is_cjk, token_stream, tokens};
pub use plan::{plan, Mode, Plan};
pub use rrf::{fuse, Fused, RRF_K};
