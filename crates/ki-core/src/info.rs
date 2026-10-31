//! 库信息：一个**只读快照**，用来回答「这个库有多大、索引好了没」。
//!
//! # 它不是什么
//!
//! - **不是设置页**。这里没有任何写入路径，也不预留。一个面板一旦既能看又能
//!   改，它的每个数字都要开始处理「改到一半」的中间态（B-29 的清理策略就是
//!   因为这件事被卡在设置页上）。
//! - **不是统计报表**。没有趋势、没有历史、没有导出。四个数就是四个数。
//!
//! # 数字的口径（每个都必须说清，否则面板会变成新的误解来源）
//!
//! | 字段 | 数的是什么 | 容易错在哪 |
//! |---|---|---|
//! | `index_bytes` | `<库>/.knowisle/` 下 `index.sqlite` 及其 `-wal` / `-shm` 的字节和 | 只算主库文件：正在建的库主库可能还是 0，而 WAL 里躺着几十 MB |
//! | `trash_bytes` | `<库>/.knowisle/trash/` 里**文件**的字节和 | 把 `trash-manifest/` 算进来：那是元数据，不是用户以为「删了但还占着」的东西 |
//! | `trash_entries` | 同一目录里的**文件数** | 与 `trash_list.total` 必须同口径（都是回收目录里的一次 `read_dir` 快照），否则面板说 3、回收站说 4 |
//! | `index_state` | 后台索引建到哪 | 复用检索那边同一个 [`IndexState`]，不另立一套 |
//!
//! `index_bytes` 与 `trash_bytes` **不含**笔记本身的体积：那是用户的文件，
//! 不是这个应用占的空间。把它们加进来会让「库信息」看起来像「磁盘占用」。

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::IndexState;

/// 一次只读快照。**没有任何缓存**：每次调用都重新读盘。
///
/// 缓存会让「我刚清空了回收站，怎么还显示 200 MB」变成一个必须靠重启解决的
/// 问题。这几个数的成本是「两次 `read_dir` + 若干次 `metadata`」，回收目录与
/// `.knowisle/` 都是平铺的小目录，不值得为它引入失效逻辑。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct VaultInfo {
    /// `.knowisle/` 下索引库占的字节数：`index.sqlite` + `-wal` + `-shm`。
    ///
    /// 三个一起算，因为 WAL 模式下它们**是同一个库**：只算主库文件时，一个
    /// 刚建到一半的库会显示 0，而磁盘上明明躺着几十 MB 的 WAL。
    pub index_bytes: u64,

    /// 回收目录里文件占的字节数。**不含** `trash-manifest/`。
    ///
    /// 这是用户唯一能感知的那部分：「我删了 500 MB，磁盘没变宽」说的就是它。
    pub trash_bytes: u64,

    /// 回收目录里的**文件数**，与 `trash_list.total` 同一口径。
    ///
    /// 面板显示它、回收站也显示它，两处必须相等——否则用户会以为其中一个坏了。
    pub trash_entries: u64,

    /// 后台索引建到哪。
    ///
    /// 与检索、跳过清单、待转存用的是同一个 [`IndexState`]：同一件事不该有
    /// 两种说法（`building` 时「搜到的可能不全」这句话在四个地方都要成立）。
    pub index_state: IndexState,
}
