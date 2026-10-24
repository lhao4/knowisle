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
use specta::Type;

/// 领域错误。变体数量刻意保持很少：错误分类的粒度应该由**处理方式**决定，
/// 而不是由"出错的地方"决定。
///
/// # 为什么 derive `Type`
///
/// 前端要写 `switch (err.code)`，就必须知道 `code` 的全部取值以及每个取值下
/// `detail` 的形状。手写这份联合类型等于把错误模型抄了两遍；`Type` 让它只有一份。
/// 新加一个变体而前端没跟上时，`tsc` 会直接报错，而不是在运行时落到 default 分支。
///
/// # 两套 rename
///
/// `rename_all` 管**变体名**（`Io` → `"IO"`），`rename_all_fields` 管**字段名**
/// （`expected_mtime` → `"expectedMtime"`）。两者分开是因为变体要 SCREAMING_SNAKE
/// 才好在 `switch` 里读，而字段要和其它所有类型一样是 camelCase。
#[derive(Debug, Clone, thiserror::Error, Serialize, Type)]
#[serde(
    tag = "code",
    content = "detail",
    rename_all = "SCREAMING_SNAKE_CASE",
    rename_all_fields = "camelCase"
)]
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

    /// 写入时发现磁盘上的文件已被别的程序改过（乐观锁失败）。
    ///
    /// 调用方读到的是 `expected_mtime` 那一版，磁盘上现在是 `disk_mtime`
    /// （`None` 表示文件已经不存在了）。
    ///
    /// **抛出本错误时文件一个字节都没被改**：检查发生在写盘之前，
    /// 磁盘保持原样，由调用方决定"重新加载"还是"强制覆盖"。
    /// 这条语义很重要——如果先写再报冲突，那冲突就成了"覆盖之后的通知"。
    #[error("文件已被外部修改：{path}")]
    Conflict {
        path: String,
        expected_mtime: i64,
        disk_mtime: Option<i64>,
    },

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
