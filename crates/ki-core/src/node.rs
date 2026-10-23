//! 领域数据结构：Vault 树、笔记内容、扫描结果。

use serde::{Deserialize, Serialize};

/// 树节点类型。序列化为小写字符串：`"dir"` / `"file"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Dir,
    File,
}

impl NodeKind {
    pub fn is_dir(self) -> bool {
        matches!(self, NodeKind::Dir)
    }
}

/// Vault 树中的一个节点。
///
/// # 不变量
///
/// - `path` 永远是 **Vault 相对路径**，且使用 POSIX 分隔符 `/`。
///   这是全链路唯一的路径表示：Rust 产出它，前端消费它，前端**不得**自行拼接。
///   违反此条会在 Windows 上产生 `notes\a.md` 与 `notes/a.md` 两套互不相等的 key。
/// - `name` 是路径最后一段的原始文本，完整保留中文与 emoji。
/// - `children` 仅目录有值；文件恒为 `None`（而不是 `Some(vec![])`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileNode {
    pub name: String,
    pub path: String,
    pub kind: NodeKind,
    /// 字节数；目录恒为 0。
    pub size: u64,
    /// Unix 毫秒时间戳；目录恒为 0。
    pub mtime: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<FileNode>>,
}

impl FileNode {
    pub fn dir(name: impl Into<String>, path: impl Into<String>, children: Vec<FileNode>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            kind: NodeKind::Dir,
            size: 0,
            mtime: 0,
            children: Some(children),
        }
    }

    pub fn file(name: impl Into<String>, path: impl Into<String>, size: u64, mtime: i64) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            kind: NodeKind::File,
            size,
            mtime,
            children: None,
        }
    }

    /// 目录且没有任何子节点。
    pub fn is_empty_dir(&self) -> bool {
        self.kind.is_dir()
            && self
                .children
                .as_ref()
                .is_none_or(|children| children.is_empty())
    }
}

/// 一次 Vault 扫描的结果。
///
/// 除了树本身，还带上足够多的诊断信息——这样"为什么少了几百个文件"
/// 这类问题可以在 UI 上直接回答，而不必让用户去翻日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    /// 规范化后的绝对路径（Windows 上已剥离 `\\?\` 前缀）。
    pub root: String,
    pub tree: FileNode,
    pub total_files: usize,
    pub total_dirs: usize,
    /// 是否因为达到文件数上限而提前停止。
    pub truncated: bool,
    /// 被跳过的条目数：权限不足、非 UTF-8 文件名、管道/设备文件等。
    pub skipped: usize,
    /// 扫描耗时（毫秒），用于直接验证 M0 的 500ms 预算。
    pub elapsed_ms: u64,
}

/// 单篇笔记的读取结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteContent {
    /// Vault 相对路径（POSIX），与请求时传入的一致。
    pub path: String,
    /// 已去除 UTF-8 BOM 的正文。
    pub content: String,
    pub size: u64,
    pub mtime: i64,
}
