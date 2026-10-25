//! 全文搜索的领域类型。
//!
//! 与 [`crate::node`] 分开放：那些回答"库里有什么"，这些回答"一次查询返回什么"。
//! 两者的生命周期不同——树在切库时换，结果每次按键都换。

use serde::{Deserialize, Serialize};
use specta::Type;

/// 一条命中：某篇笔记的某一行。
///
/// # 两套偏移量，不是冗余
///
/// `from` / `to` 是相对 [`SearchHit::text`] 的下标，前端直接 `text.slice(from, to)`
/// 就能把命中片段包进 `<mark>`；`column` / `column_end` 是相对**整行**的位置，
/// 编辑器跳转用它。
///
/// 分开的原因是 `text` 可能是一个**窗口**：超长行只截命中附近的一段返回
/// （否则一条 5000 字的长段落会把结果列表撑成一面墙，payload 也会失控）。
/// 若只给一套偏移，前端就得自己知道窗口从哪儿开始——那种"约定"迟早会算错。
///
/// 所有偏移的单位都是 **UTF-16 码元**，与 JavaScript 的字符串下标、以及
/// CodeMirror 6 的文档位置完全一致。Rust 侧的字节偏移在出口处就换算掉。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// Vault 相对路径（POSIX），与 `note_read` 接受的形式一致。
    pub path: String,
    /// 1-based 行号。
    pub line: u32,
    /// 该行的展示文本。通常是整行；超长行会被裁成命中附近的一个窗口。
    pub text: String,
    /// 命中在 `text` 里的起点（UTF-16 码元）。
    pub from: u32,
    /// 命中在 `text` 里的终点（UTF-16 码元，不含）。
    pub to: u32,
    /// 命中在整行里的起点（UTF-16 码元）。
    pub column: u32,
    /// 命中在整行里的终点（UTF-16 码元，不含）。
    pub column_end: u32,
    /// 这篇笔记的**文件名**也包含关键词。排序时这类命中排在前面。
    pub name_hit: bool,
}

/// 一次全文搜索的结果。
///
/// # 四个诊断字段在 M4 换引擎之后语义变了
///
/// 它们都是给 UI 与诊断用的，不影响命中内容。换引擎（内存子串 → FTS5 索引）
/// 之后它们的**来源**变了，但用户看到的行为不变：
///
/// | 字段 | M2（内存引擎） | M4（FTS5 引擎） |
/// |---|---|---|
/// | `indexed_files` | 本次建索引成功读取的篇数 | 索引库里 `status='ok'` 的行数 |
/// | `skipped_files` | 本次建索引跳过的篇数 | 索引库里 `status='skipped'` 的行数 |
/// | `index_ms` | 本次请求建索引的耗时 | **本次搜索之前最近一次**索引维护的耗时 |
/// | `builds` | 内存索引被建起来的次数 | 索引库被打开 / 建立的次数 |
///
/// 两个 `*_files` 变成"库里的计数"是必须的：FTS 引擎不会在查询时重新读盘，
/// 所以它根本不知道"本次读了几篇"。这也让它们更准确——跳过一篇大文件之后，
/// 之后的每次搜索都会如实报告它被跳过了，而不是只在建索引的那一次报告。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// 命中行，已排序并截断到 200 条。
    pub hits: Vec<SearchHit>,
    /// 命中条数（是**行数**，不是文件数）。UI 用它显示「共 N 条」。
    ///
    /// FTS 引擎下它是**候选池内**的命中行数。候选池饱和（某条通道取满了上限）
    /// 时它是**下界**，不是准确值——`contentless` 的 FTS 表不存正文，
    /// 要数全库就得读全库。此时 [`SearchResult::truncated`] 为真，
    /// UI 必须把「共 N 条」改说成「至少 N 条」。
    pub total: usize,
    /// 是否发生了截断。两种情况：命中超过 200 条，或者候选池饱和。
    pub truncated: bool,
    /// 索引里的文件数（`status='ok'` 的行数），不是本次读取的篇数。
    pub indexed_files: usize,
    /// 索引时跳过的文件数（`status='skipped'`）：非 UTF-8、超过 2MB、IO 失败。
    pub skipped_files: usize,
    /// **本次搜索之前最近一次**索引维护的耗时（毫秒）：打开 / 重建索引库、
    /// 扫描后的增量同步、刷新当前编辑篇，三者之和。被取走一次后清零，
    /// 所以连打 3 个字符只有第一次非 0。
    pub index_ms: u64,
    /// 当前 Vault 的索引库在本进程被打开 / 建立的次数。
    ///
    /// 切库归零，重新扫描（缓存作废）后继续累加。
    ///
    /// 诊断字段，不是业务字段：连打 3 个字符会发 3 次搜索，这个数必须恒为 1。
    /// 它也是唯一能从外部证明"锁真的生效了"的证据。
    pub builds: u32,
}
