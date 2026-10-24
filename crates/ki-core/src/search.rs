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
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// 命中行，已排序并截断到 200 条。
    pub hits: Vec<SearchHit>,
    /// 截断前的命中条数（是**行数**，不是文件数）。UI 用它显示「共 N 条」。
    pub total: usize,
    /// 是否因为超过 200 条而截断。
    pub truncated: bool,
    /// 进入索引的文件数（建索引时成功读取的篇数）。
    pub indexed_files: usize,
    /// 建索引时跳过的文件数：非 UTF-8、超过 2MB、IO 失败。
    pub skipped_files: usize,
    /// 本次请求建索引的耗时（毫秒）。命中缓存时为 0。
    pub index_ms: u64,
    /// 当前 Vault 的索引被建起来的次数。
    ///
    /// 切库归零，重新扫描（缓存作废）后继续累加。
    ///
    /// 诊断字段，不是业务字段：连打 3 个字符会发 3 次搜索，这个数必须恒为 1。
    /// 它也是唯一能从外部证明"锁真的生效了"的证据。
    pub builds: u32,
}
