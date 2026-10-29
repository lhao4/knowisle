//! 待转存集合与批量转存的领域类型。
//!
//! # 为什么在 `ki-core` 而不是 `ki-vault`
//!
//! 它们跨 IPC 出去（前端要渲染清单、进度、失败逐条），理由与
//! [`crate::trash`] 完全相同：放 `ki-vault` 就得在 `ki-ipc` 里写一份镜像 DTO。
//! 它们没有 IO，不违反 A2。
//!
//! # 这里没有「编码探测」
//!
//! 编码的**判定**只有一处：`ki_vault::encoding::decode`（ADR-010）。
//! 本模块只搬运它的结论（一个已经算好的编码名），不做任何判断。
//! 待转存集合的来源是索引库的那一列，不是重新扫盘——见 ADR-012。

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::{IndexState, KiError};

/// 待转存清单里的一条。
///
/// `encoding` 是**显示名**（`GBK` / `Big5` / `UTF-16LE`），由
/// `ki_vault::encoding::Encoding` 给出，与编辑器顶部那行「已按 X 解码」
/// 用的是同一个来源。两处必须一致：用户在清单里看到 `GBK`，点开那篇
/// 也必须是 `GBK`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EncodingEntry {
    /// Vault 相对路径（POSIX）。
    pub path: String,
    /// 磁盘上实际的编码名。
    pub encoding: String,
}

/// 库级编码统计：**不打开任何一篇**就能知道库里有多少篇不是 UTF-8。
///
/// # `total` 是精确值，`truncated` 只描述清单
///
/// 检索那边的 `truncated` 表示「`total` 是下界」，因为候选池按预算截断后
/// 不再逐篇数。这里不一样：`count(*)` 是索引上的常数级查询，**永远精确**，
/// 没有理由让它变成下界。所以：
///
/// | 字段 | 含义 |
/// |---|---|
/// | `total` | 待转存的篇数，**精确** |
/// | `non_utf8` | 按路径排序的清单，最多 `limit` 条 |
/// | `truncated` | 清单被截断了（`non_utf8` 只是前 `limit` 条） |
///
/// 两者共有的语义只有一条：**`truncated` 为真时，别把清单长度当成全部**。
/// UI 因此可以一直显示精确的「N 篇不是 UTF-8」，同时在清单底部说
/// 「只列出前 N 篇」。
///
/// # 为什么不给「UTF-8 篇数」
///
/// 前端要的句子是「N 篇不是 UTF-8」。分母（库里一共几篇）侧栏已经有
/// （扫描结果），再给一个会在两次刷新之间与树对不上。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EncodingStats {
    /// 待转存篇数（精确）。
    pub total: usize,
    /// 待转存清单，按路径升序。最多 `ENCODING_LIST_MAX` 条。
    pub non_utf8: Vec<EncodingEntry>,
    /// 清单是否被截断。
    pub truncated: bool,
    /// 这份统计**依据的那份索引**建到哪了。
    ///
    /// # 为什么必须有这个字段
    ///
    /// 数字来自索引。索引在后台建（10k 篇 10–108 秒），建到一半时 `total`
    /// 是「目前为止看到的篇数」——它会被读成「库里只有 5 篇不是 UTF-8」。
    /// 而索引库刚被重建（schema 升级、换库、库损坏）时更糟：那一刻 `total`
    /// 是 0，而用户的库里明明躺着几百篇 GBK。
    ///
    /// 有了它，前端能说「索引还在建，这个数会长」并继续等，而不是把
    /// 「还没数到」当成「没有」——后者会让入口**永远不出现**。
    ///
    /// 复用检索那边同一个 [`IndexState`]：同一件事（索引建到哪）不该有
    /// 第二套枚举。
    pub index_state: IndexState,
}

impl EncodingStats {
    /// 一篇都没有时的形状。UI 用它判断「入口要不要出现」。
    pub fn empty(index_state: IndexState) -> Self {
        Self {
            total: 0,
            non_utf8: Vec::new(),
            truncated: false,
            index_state,
        }
    }

    /// 有没有待转存的篇。**这是入口是否出现的唯一判据**——
    /// 不许用「有没有打开过库」「索引建完没有」之类的近似。
    ///
    /// 索引在建时返回 `false` 是刻意的：入口一旦出现就不该再消失
    /// （那会让人以为自己看错了），所以只在**确定有**时才出现。
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

/// 批量转存里的一篇失败。
///
/// `error` 是完整的 [`KiError`]，不是一句拼好的文案：前端已经有
/// `describeError`（13 个变体逐个分支，末尾 `never` 兜底），在这里再拼一遍
/// 等于把错误模型抄第二份，且抄漏的那一支会静默落到「未知错误」。
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeFailure {
    pub path: String,
    pub error: KiError,
}

/// 一次批量转存的结果。
///
/// # 三件事必须分开说
///
/// - `ok`：真的被改写成 UTF-8 的篇（**不含**本来就是 UTF-8 的幂等篇——
///   它们根本不会进清单，见 `EncodingStats`）；
/// - `failed`：逐条带路径与原因，**不是**一个总数；
/// - `cancelled`：用户中途点了取消。已转好的**不回滚**（`history/` 里有备份，
///   且转存是幂等的），所以 `ok` 仍然要如实给出。
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeBatch {
    pub ok: Vec<String>,
    pub failed: Vec<TranscodeFailure>,
    /// 是否被用户取消。取消后 `ok + failed` 只覆盖跑过的那一段。
    pub cancelled: bool,
}

impl TranscodeBatch {
    pub fn empty() -> Self {
        Self {
            ok: Vec::new(),
            failed: Vec::new(),
            cancelled: false,
        }
    }
}

/// 批量转存的进度事件载荷。
///
/// # 为什么进度从 Rust 来
///
/// 前端按「发了几个请求」猜进度是不准的：一次 IPC 调用跑完 1000 篇，
/// 中间没有任何可观测的点。逐篇 emit 是唯一能反映**真实**进度的做法
/// （也顺带让「卡在第 3 篇」这种失败可见）。
///
/// 事件名见 `ki_ipc::commands::TRANSCODE_PROGRESS_EVENT`。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeProgress {
    /// 本次批量的标识。前端拿它过滤掉上一轮（或别的窗口）的事件。
    pub token: u64,
    /// 已经处理完的篇数（含失败）。
    pub done: usize,
    /// 本次要处理的总篇数。
    pub total: usize,
    /// 刚处理完的那一篇。
    pub path: String,
    /// 到目前为止失败的篇数。
    pub failed: usize,
}
