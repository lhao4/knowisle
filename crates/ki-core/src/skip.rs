//! 跳过清单：**诊断数据，不是状态**（ADR-013）。
//!
//! # 两个 `skipped` 不是一件事
//!
//! | 出处 | 数的是 | 成员 | 谁写的 |
//! |---|---|---|---|
//! | [`ScanSkipEntry`] | 扫描时跳过的**条目**（目录、软链、FIFO、非 UTF-8 文件名） | 磁盘上的目录项 | `ki_vault::scan_vault` |
//! | [`IndexSkipEntry`] | 索引时跳过的**笔记**（认不出编码、超过 2MB、IO 失败） | `note.status = 'skipped'` 的行 | `ki_ipc::search` 的同步 |
//!
//! 两者**任何时候都不相等也合法**：一篇 3MB 的 `.md` 在扫描侧是一棵完全正常
//! 的树节点，只有索引侧才跳过它；反过来，一个没有读权限的目录在扫描侧计一条，
//! 索引侧根本看不见它。把它们相加、取最大、或者只显示其中一个，都会让
//! 「侧栏说跳过 3、搜索说跳过 1」这件事重新变成一句无法解释的话。
//!
//! # 为什么不落盘
//!
//! 清单描述的是**这一次**扫描 / 这一次索引看到了什么。落盘就是第二份真相源：
//! 用户把文件挪走之后，落盘的清单还写着它，而「点进去看看」只会打开一个
//! 不存在的路径。索引侧的行本来就在 `note` 表里（那是索引的派生数据），
//! 扫描侧的清单每次扫描重新生成，两边的生命周期都与「数据从哪来」一致。
//!
//! # 与 `EncodingStats` 的关系
//!
//! `EncodingStats`（ADR-012）回答「哪几篇需要转存」——那些篇**能被正常读取**，
//! 只是磁盘上的字节不是 UTF-8，动作是「转存」。本模块的
//! [`IndexSkipReason::InvalidUtf8`] 回答的是另一件事：**正文认不出编码**，
//! 读都读不出来，动作是「用户自己看一眼」。两个名字像、含义完全不同，
//! 文案也必须分开写——`InvalidUtf8` 是正文，`NonUtf8Name` 是文件名。

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::IndexState;

/// 扫描时被跳过的一条。
///
/// `path` 是 `Option`：`walkdir` 的错误分支可能连路径都拿不到（`Loop` 变体
/// 只给祖先与子节点，不给一个"这条路径"）。清单里必须能渲染「没有路径」，
/// 不许 `unwrap`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanSkipEntry {
    /// Vault 相对路径（POSIX）。`None` = 连路径都没拿到。
    pub path: Option<String>,
    /// `path` 是不是 `to_string_lossy()` 的产物。
    ///
    /// # 为什么必须单独一个布尔值
    ///
    /// 非 UTF-8 文件名那一档**拿不到真路径**——`to_str()` 返回 `None` 正是
    /// 它被跳过的原因。清单里仍然要给出看得见的一行，于是只能 lossy；
    /// 而 lossy 会把无法表示的字节换成 `U+FFFD`，**用户照着它去改名会找不到
    /// 文件**。所以这一位不是装饰：它为真时 UI 必须加一句「名字含无法显示的
    /// 字符」，让用户知道这个名字不能照抄。
    ///
    /// 为假时 `path` 是逐字可信的原始路径。
    pub path_lossy: bool,
    pub reason: ScanSkipReason,
}

/// 扫描侧为什么跳过。**每一种对应 `scan_vault` 里的一处 `record`。**
///
/// # 为什么不与索引侧的 `SkipReason` 合成一个枚举
///
/// 两边至少差三种只有一侧才有的原因（非 UTF-8 **文件名**、`walkdir` 错误、
/// `strip_prefix` 失败）。合成一个枚举的结果是每一侧都要写「这一支我永远不会
/// 产生」的分支，而那种分支最先腐烂——半年后没人敢删。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ScanSkipReason {
    /// 目录或文件没有读权限（`PermissionDenied`）。
    Permission,
    /// 文件名里有无法用 UTF-8 表示的字节（Linux 上由任意字节组成的名字）。
    NonUtf8Name,
    /// 既不是目录也不是普通文件：管道、socket、设备文件、**软链**。
    ///
    /// 软链也在这里：`follow_links(false)` 下它是一个独立的目录项，
    /// 但既不是 `is_dir()` 也不是 `is_file()`。
    NotARegularFile,
    /// `walkdir` 报了错，但不是权限问题（遍历中途文件消失、目录被并发删除、
    /// 软链成环）。**用户多半修不了**，但仍然给路径：文件被删了这件事，
    /// 说出来比不说好。
    WalkError,
    /// 内部不变量被破坏（`strip_prefix` 失败、文件名与整条路径的 UTF-8 判定
    /// 自相矛盾）。**永不进清单**，只进 `skipped` 总数——用户修不了的东西
    /// 列在他面前，只会让他以为能修。
    Internal,
}

/// 索引时被跳过的一条。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IndexSkipEntry {
    /// Vault 相对路径（POSIX）。索引侧的行来自扫描树，所以**恒有路径**。
    pub path: String,
    pub reason: IndexSkipReason,
}

/// 索引侧为什么跳过。
///
/// 与 `ki_store::SkipReason` 是**同一件事的两个形状**：那边是落库的字符串
/// （`too_large` / `invalid_utf8` / `io`），这边是跨 IPC 的枚举。多出来的
/// [`IndexSkipReason::Unknown`] 只可能来自一个被外部改过的索引库——它存在是
/// 为了「`total` 与清单长度不许对不上」：认不出的原因也要占一行，否则
/// `count(*)` 说 3、清单只有 2 条，而 `truncated` 是 `false`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum IndexSkipReason {
    /// 超过单篇索引上限（2MB）。
    TooLarge,
    /// **正文**认不出编码：BOM、NUL 模式、严格 UTF-8、chardetng 四道都过了
    /// 还是解不出来。
    ///
    /// 与 [`ScanSkipReason::NonUtf8Name`] 不是一件事：那是**文件名**，
    /// 这是**正文**。一篇 GBK 的笔记在这里**不会**出现（它解得出来，
    /// 只是需要转存，见 `EncodingStats`）。
    InvalidUtf8,
    /// 读取时 IO 失败。
    Io,
    /// 索引里记着一个认不出的原因（索引库被外部改过）。
    Unknown,
}

/// 索引侧跳过清单。
///
/// # `total` 精确，`truncated` 只描述清单
///
/// 与 [`crate::EncodingStats`] 逐字同一条契约（ADR-012），不重新发明：
///
/// | 字段 | 含义 |
/// |---|---|
/// | `total` | 跳过的**笔记**数，精确（`count(*)`） |
/// | `entries` | 按路径升序的清单，最多 `SKIP_LIST_MAX` 条 |
/// | `truncated` | 清单被截断了（`entries` 只是前 N 条） |
///
/// UI 一直显示精确的 `total`，只在清单底部说「只列出前 N 篇」。
/// **别拿清单长度当总数**——那是第十一刀专门写进 ADR 的一句话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IndexSkipStats {
    /// 跳过的笔记数（精确）。
    pub total: usize,
    /// 清单，按路径升序。最多 `SKIP_LIST_MAX` 条。
    pub entries: Vec<IndexSkipEntry>,
    /// 清单是否被截断。
    pub truncated: bool,
    /// 这份统计**依据的那份索引**建到哪了。
    ///
    /// # 为什么必须有这个字段
    ///
    /// 索引在后台建（10k 篇 10–108 秒）。建到一半时 `total` 是「目前为止
    /// 数到的」；索引库刚被重建（schema 升级、换库、库损坏）那一刻更糟：
    /// `total` 是 0，而用户的库里明明躺着几篇 3MB 的笔记。前端据此继续轮询，
    /// 而不是把 0 读成「没有跳过」——那会让入口**永远不出现**。
    ///
    /// 第十一刀已经踩过这个坑（ADR-012 的「`indexState` 必须跟着数字一起
    /// 回来」），这是第二次，所以复用它，不另立枚举。
    pub index_state: IndexState,
}

impl IndexSkipStats {
    /// 一篇都没有时的形状。
    pub fn empty(index_state: IndexState) -> Self {
        Self {
            total: 0,
            entries: Vec::new(),
            truncated: false,
            index_state,
        }
    }
}
