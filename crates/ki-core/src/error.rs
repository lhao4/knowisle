//! 跨 IPC 的统一错误模型。
//!
//! 序列化形态是 adjacently tagged：
//!
//! ```json
//! {
//!   "code": "IO",
//!   "detail": { "path": "notes/a.md", "kind": "PERMISSION_DENIED", "message": "拒绝访问" }
//! }
//! ```
//!
//! 前端必须用 `switch (err.code)` 分支处理，**禁止**解析 `message` 字符串——
//! 那是把国际化、日志格式与用户文案三者耦合在一起的经典错误。`IO` 的细分同理：
//! 看 `kind`，不要对 `message` 做 `includes`。

use std::io::ErrorKind;

use serde::Serialize;
use specta::Type;

/// 领域错误。变体数量刻意保持很少：错误分类的粒度应该由**处理方式**决定，
/// 而不是由"出错的地方"决定。
///
/// M3 加 [`KiError::NotAFile`] / [`KiError::ReservedPath`] 时，这条规矩被具体化
/// 了一次：它们不是"另一个出错位置"，而是两种**下一步动作不同**的拒绝——
/// 前者是选错了对象（去选文件），后者是这块地方根本不许动（去选库里的笔记）。
/// 判据始终是"前端要不要给出不同的话"，不是"错误发生在哪个函数里"。
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
/// IO 失败的**可操作分类**。
///
/// 分类粒度由「用户下一步该做什么」决定，而不是由 errno 决定：
/// 改权限 / 关掉占用程序 / 清磁盘 / 重新扫描 / 没有建议。
/// 这是 [`KiError`] 那条「粒度按处理方式」的规矩在 IO 里的落地。
///
/// # 为什么是这五个
///
/// - `PermissionDenied`：用户能修（改 ACL、去掉只读、换目录）。
/// - `Busy`：用户能修，但动作是**关掉别的程序**而不是改权限。
/// - `StorageFull`：用户能修，但要清磁盘；且此刻内容还没落盘，文案不能许诺安全。
/// - `NotFound`：用户能修，动作是**重新扫描**（磁盘上确实没了，重试没有意义）。
/// - `Other`：没有可执行建议，只能把 OS 原文摆出来。
///
/// 序列化是 `SCREAMING_SNAKE_CASE`（`PERMISSION_DENIED` / `BUSY` / …），
/// 与 `code` 的写法一致，前端 `switch` 里读起来不分心。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IoKind {
    /// 没有权限（ACL、只读、系统保护）。
    PermissionDenied,
    /// 文件被别的进程占用 / 锁定。
    Busy,
    /// 磁盘空间不足。
    StorageFull,
    /// 文件或路径已经不存在。
    NotFound,
    /// 无法归类的 IO 失败。
    Other,
}

impl IoKind {
    /// 从 `std::io::Error` 判定分类。
    ///
    /// # 顺序不能换
    ///
    /// 先看 [`ErrorKind`]（跨平台稳定语义），再看 raw os code（平台补丁）。
    /// Windows 上 `ERROR_SHARING_VIOLATION`（32）落到 `Uncategorized` 而不是
    /// `PermissionDenied`（已实测），所以这条顺序不会把「被占用」误判成「没权限」。
    ///
    /// # 为什么不匹配 `ErrorKind::StorageFull` / `ResourceBusy`
    ///
    /// 它们是 Rust **1.83** 才稳定的（`io_error_more`），而本 crate 的 MSRV 是
    /// **1.82**。在本机（1.99）写它们编得过，CI 也不查 MSRV，于是会形成一条
    /// 不报错的隐性违约。所以磁盘满与占用只走 raw code 表，`ErrorKind` 分支
    /// 只匹配 1.0 就有的 `PermissionDenied` / `NotFound`，其余 `_ =>` 落到下表。
    pub fn from_io(err: &std::io::Error) -> Self {
        if err.kind() == ErrorKind::PermissionDenied {
            return Self::PermissionDenied;
        }
        if err.kind() == ErrorKind::NotFound {
            return Self::NotFound;
        }

        // 两张表必须分开：Unix 的 5 是 EIO，Windows 的 5 是 ACCESS_DENIED。
        // 共用一张表会把 EIO 说成「权限不足」，让用户去改一个没问题的权限。
        match err.raw_os_error() {
            #[cfg(windows)]
            Some(5) => Self::PermissionDenied, // ERROR_ACCESS_DENIED
            #[cfg(windows)]
            Some(32) | Some(33) => Self::Busy, // SHARING_VIOLATION / LOCK_VIOLATION
            #[cfg(windows)]
            Some(112) | Some(39) => Self::StorageFull, // DISK_FULL / HANDLE_DISK_FULL

            #[cfg(unix)]
            Some(13) => Self::PermissionDenied, // EACCES
            #[cfg(unix)]
            Some(16) | Some(26) => Self::Busy, // EBUSY / ETXTBSY
            #[cfg(unix)]
            Some(28) => Self::StorageFull, // ENOSPC

            _ => Self::Other,
        }
    }
}

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

    /// 传入的路径存在，但不是普通文件（目录、设备、FIFO 等）。
    ///
    /// 与 [`KiError::NotADirectory`] 对称：那是「要目录却给了文件」，
    /// 这是「要文件却给了目录」。两者都是**选错对象**，不是磁盘出错——
    /// 所以不能塞进 [`KiError::Io`]，否则前端只能显示「读写失败」，
    /// 用户会去查权限和磁盘，而真正该做的是换一个目标。
    #[error("不是普通文件：{path}")]
    NotAFile { path: String },

    /// 路径试图逃出 Vault（`..`、绝对路径、盘符、符号链接指向外部）。
    #[error("路径越界：{path}")]
    PathEscape { path: String },

    /// 路径落在应用自己的元数据目录（`.knowisle/`）里。
    ///
    /// 这是**规则拒绝**，不是磁盘故障：文件完全可读可写，只是不许改名 / 删除。
    /// 单独成变体是为了让前端说「这里不能动」，而不是让用户去查权限。
    #[error("{path} 是应用自己的元数据，不能改名或删除")]
    ReservedPath { path: String },

    /// 文件系统读写失败。
    ///
    /// `kind` 是给用户下一步动作的分类；`message` 是 OS 原文，只在
    /// [`IoKind::Other`] 时作为补充细节展示。**禁止**解析 `message` 做分支——
    /// 它随平台与语言变化，不构成契约。
    #[error("IO 错误（{path}）：{message}")]
    Io {
        path: String,
        kind: IoKind,
        message: String,
    },

    /// 文件名或文件内容不是合法 UTF-8。
    ///
    /// 「合法 UTF-8」这句话在 M7 第八刀之后已经不准了：内容侧现在会探测
    /// GBK / UTF-16 / Big5 等编码，只有**全都认不出来**时才落到这里。
    /// 文件名侧没有变——文件名非 UTF-8 仍然只计入 `skipped`。
    #[error("编码错误：{path}")]
    InvalidEncoding { path: String },

    /// 正文里有原编码表示不了的字符，或原文件在该编码下无法无损回写。
    ///
    /// 这是**拒绝保存**，不是读取失败：抛出本错误时磁盘上一个字节都没动。
    /// 单独成变体而不是塞进 [`KiError::InvalidEncoding`]，是因为下一步动作
    /// 完全不同——那里是「这篇打不开」，这里是「这篇能打开、能编辑，
    /// 但你不能把它存回原来的编码」。硬塞进前者，用户会以为文件坏了。
    ///
    /// `reason` 由 Rust 给出（「GBK 里没有 😀 的字节」这类），前端直接展示。
    /// 规则只有一份，文案也就不该有第二份。
    #[error("无法按 {encoding} 保存（{path}）：{reason}")]
    EncodingNotWritable {
        path: String,
        encoding: String,
        reason: String,
    },

    /// 文件名不合法（保留名、非法字符、全角标点、超长、空名等）。
    ///
    /// `reason` 是给用户看的一句话，必须能指导下一步动作（"请用半角"），
    /// 而不是复述"不合法"。前端直接展示它，不做二次翻译——
    /// 判定规则只有 Rust 一份，文案也就不该有第二份。
    #[error("文件名不合法（{name}）：{reason}")]
    InvalidName { name: String, reason: String },

    /// 目标名字已被占用：新建时撞名，或重命名到一个已存在的文件。
    ///
    /// **抛出本错误时磁盘上什么都没变**：新建是 `create_new` 失败，
    /// 重命名在动目标之前就返回了。调用方可以放心地让用户改个名字重试。
    #[error("目标已存在：{path}")]
    NameTaken { path: String },

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

    /// 本地索引库不可用（打开失败、损坏、磁盘满、被占用超时）。
    ///
    /// 索引是**派生数据**（公理 A1）：删掉 `.knowisle/index.sqlite` 后重新打开库，
    /// 它会自动重建。所以它不能塞进 [`KiError::Io`]——`IoKind` 的五种分类对应的是
    /// 「改权限 / 关掉占用程序 / 清磁盘 / 重新扫描 / 没有建议」，而这里该做的是
    /// **第六种动作：删掉索引让它重建**。硬塞进 `Io` 只会让前端给出错误的建议。
    ///
    /// 也不能塞进 [`KiError::Internal`]：那是「不该发生的 bug」，而索引损坏是
    /// 可预期、可自愈的。
    #[error("索引库不可用（{path}）：{message}")]
    Index { path: String, message: String },

    /// 不应发生的内部错误（后台任务 panic、序列化失败等）。
    #[error("{0}")]
    Internal(String),
}

impl KiError {
    /// 从 `std::io::Error` 构造，自动带上出错路径与可操作分类。
    ///
    /// 签名刻意保持不变：23 个调用点不需要知道 [`IoKind`] 的存在，
    /// 分类是构造时顺手完成的事实提取，不是调用方的责任。
    pub fn io(path: impl Into<String>, err: &std::io::Error) -> Self {
        KiError::Io {
            path: path.into(),
            kind: IoKind::from_io(err),
            message: err.to_string(),
        }
    }

    /// 同 [`KiError::io`]，但在 OS 原文前加一句**本操作特有**的后果说明。
    ///
    /// 只给「失败之后磁盘处于什么状态」需要额外交代的路径用（例如删除失败时
    /// 文件仍在原位、绝不降级成永久删除）。普通读写不要用：前缀会稀释 OS 原文，
    /// 而用户真正需要的是错误码对应的动作，不是又一句解释。
    pub fn io_msg(path: impl Into<String>, err: &std::io::Error, prefix: &str) -> Self {
        KiError::Io {
            path: path.into(),
            kind: IoKind::from_io(err),
            message: format!("{prefix}：{err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 直接引入变体，断言里就不必写带类型前缀的模式——那种写法在仓库里
    // 只该出现在两个构造器上（验收 A3 用 grep 数它，别让测试污染计数）。
    use KiError::Io;

    /// `ErrorKind` 是跨平台语义，必须先于 raw code 命中。
    #[test]
    fn error_kind_is_checked_before_raw_os_code() {
        let denied = std::io::Error::from(ErrorKind::PermissionDenied);
        assert_eq!(IoKind::from_io(&denied), IoKind::PermissionDenied);

        let missing = std::io::Error::from(ErrorKind::NotFound);
        assert_eq!(IoKind::from_io(&missing), IoKind::NotFound);

        // 没有 raw code 时不能 panic，也不能误判成前两类。
        let other = std::io::Error::from(ErrorKind::Other);
        assert_eq!(IoKind::from_io(&other), IoKind::Other);
    }

    /// Windows 的 raw code 表。
    ///
    /// 32 / 33 在 `ErrorKind` 里是 `Uncategorized`（已实测），所以必须靠这张表
    /// 才能落到 `Busy`；39 与 112 都是磁盘满（前者是句柄所在卷满）。
    #[cfg(windows)]
    #[test]
    fn windows_raw_codes_map_to_actionable_kinds() {
        for (code, want) in [
            (5, IoKind::PermissionDenied), // ERROR_ACCESS_DENIED
            (32, IoKind::Busy),            // ERROR_SHARING_VIOLATION
            (33, IoKind::Busy),            // ERROR_LOCK_VIOLATION
            (112, IoKind::StorageFull),    // ERROR_DISK_FULL
            (39, IoKind::StorageFull),     // ERROR_HANDLE_DISK_FULL
            (2, IoKind::NotFound),         // ERROR_FILE_NOT_FOUND
            (3, IoKind::NotFound),         // ERROR_PATH_NOT_FOUND
            (123, IoKind::Other),          // ERROR_INVALID_NAME：与上面四类都无关
        ] {
            let err = std::io::Error::from_raw_os_error(code);
            assert_eq!(IoKind::from_io(&err), want, "winerror {code}");
        }
    }

    /// Unix 的 raw code 表。
    ///
    /// 最关键的一条是 errno 5：Unix 的 5 是 **EIO**，Windows 的 5 是
    /// ACCESS_DENIED。若两张表合并，EIO 会被说成「权限不足」。
    #[cfg(unix)]
    #[test]
    fn unix_raw_codes_map_to_actionable_kinds() {
        for (code, want) in [
            (1, IoKind::PermissionDenied),  // EPERM
            (13, IoKind::PermissionDenied), // EACCES
            (16, IoKind::Busy),             // EBUSY
            (26, IoKind::Busy),             // ETXTBSY
            (28, IoKind::StorageFull),      // ENOSPC
            (2, IoKind::NotFound),          // ENOENT
            (5, IoKind::Other),             // EIO：绝不是权限问题
        ] {
            let err = std::io::Error::from_raw_os_error(code);
            assert_eq!(IoKind::from_io(&err), want, "errno {code}");
        }
    }

    /// 构造器要把 kind 与 OS 原文一起带上，且签名保持 `(path, &err)`。
    #[test]
    fn io_constructor_keeps_kind_and_os_message() {
        let err = std::io::Error::from(ErrorKind::PermissionDenied);
        match KiError::io("notes/a.md", &err) {
            Io {
                path,
                kind,
                message,
            } => {
                assert_eq!(path, "notes/a.md");
                assert_eq!(kind, IoKind::PermissionDenied);
                assert_eq!(message, err.to_string());
            }
            other => panic!("应当是 Io，实际 {other:?}"),
        }
    }

    /// `io_msg` 只在 OS 原文前加一句后果说明，不吞掉原文与分类。
    #[test]
    fn io_msg_prepends_the_consequence_note() {
        let err = std::io::Error::from(ErrorKind::PermissionDenied);
        let error = KiError::io_msg(".knowisle/trash", &err, "无法创建回收目录");

        match &error {
            Io { kind, .. } => assert_eq!(*kind, IoKind::PermissionDenied),
            other => panic!("应当是 Io，实际 {other:?}"),
        }
        let text = error.to_string();
        assert!(text.contains("无法创建回收目录："), "{text}");
        assert!(text.contains(&err.to_string()), "{text}");
    }
}
