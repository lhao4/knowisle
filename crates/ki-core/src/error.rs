//! 跨 IPC 的统一错误模型。
//!
//! 序列化形态是 adjacently tagged：
//!
//! ```json
//! { "code": "IO", "detail": { "path": "notes/a.md", "message": "拒绝访问" } }
//! ```
//!
//! 前端必须用 `switch (err.code)` 分支处理，**禁止**解析 `message` 字符串——
//! 那是把国际化、日志格式与用户文案三者耦合在一起的经典错误。

use serde::Serialize;

/// 领域错误。变体数量刻意保持很少：错误分类的粒度应该由**处理方式**决定，
/// 而不是由"出错的地方"决定。
#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "code", content = "detail", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum KiError {
    /// 传入的路径存在，但不是目录。
    #[error("不是目录：{path}")]
    NotADirectory { path: String },

    /// 路径试图逃出 Vault（`..`、绝对路径、盘符、符号链接指向外部）。
    #[error("路径越界：{path}")]
    PathEscape { path: String },

    /// 文件系统读写失败。
    #[error("IO 错误（{path}）：{message}")]
    Io { path: String, message: String },

    /// 文件名或文件内容不是合法 UTF-8。
    #[error("编码错误：{path}")]
    InvalidEncoding { path: String },

    /// 文件超出单文件大小上限。
    #[error("文件过大（{size} 字节）：{path}")]
    TooLarge { path: String, size: u64 },

    /// 不应发生的内部错误（后台任务 panic、序列化失败等）。
    #[error("{0}")]
    Internal(String),
}

impl KiError {
    /// 从 `std::io::Error` 构造，自动带上出错路径。
    pub fn io(path: impl Into<String>, err: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            message: err.to_string(),
        }
    }
}
