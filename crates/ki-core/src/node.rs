//! 领域数据结构：Vault 树、笔记内容、扫描结果。

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::error::KiError;

/// 树节点类型。序列化为小写字符串：`"dir"` / `"file"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
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
///
/// # 数值宽度
///
/// `size` 是 `u64`、`mtime` 是 `i64`，但前端拿到的是 JS `number`——
/// 这不是疏忽，是 Tauri v2 的 IPC 只走 JSON，`u64` 到了 webview 里本来就是 double。
/// 生成绑定时用 `dangerously_cast_bigints_to_number()` 承认这个既成事实；
/// 若改成 TS `bigint` 反而与运行时值不符。安全边界是 2^53：
/// 文件字节数与毫秒时间戳都不可能越界（2^53 毫秒 ≈ 28.5 万年）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
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
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    /// 规范化后的绝对路径（Windows 上已剥离 `\\?\` 前缀）。
    pub root: String,
    pub tree: FileNode,
    /// 树里的笔记数（只数 `.md` / `.markdown`，不含目录）。
    ///
    /// # `truncated` 为真时它是**下界**
    ///
    /// 截断发生在 DFS 中途，后面的条目根本没访问过，所以这个数只覆盖
    /// 「扫到的那一段」。判据与 [`crate::SearchResult::total`] 是同一条公理：
    ///
    /// > **`total_files` 不准 ⇔ [`ScanResult::truncated`] 为真。**
    ///
    /// 不许出现「`truncated` 为真而 `total_files` 恰好准确」的依赖——那只是
    /// 这一次的巧合，UI 一旦把它当准数就会在下一个库上说错。
    pub total_files: usize,
    pub total_dirs: usize,
    /// 本次扫描实际生效的条目上限（= `ScanOptions::max_entries`）。
    ///
    /// # 为什么必须从结果里来
    ///
    /// 横幅要显示「在多少个条目处停下」，而 `ScanOptions` **不进 IPC**
    /// （`vault_scan` 只收 root）。前端曾经把这个数写死成 20000，默认值改了
    /// 之后横幅继续说 20000——它已经错过一次（ADR-015）。
    ///
    /// 未截断时它也是真实值：它描述「这次扫描最多愿意收多少条」，不是
    /// 「收了多少条」（后者是 `total_files + total_dirs`）。
    pub limit_entries: usize,
    /// **路径集合不全**：扫描在 [`ScanResult::limit_entries`] 处提前停止。
    ///
    /// # 它只说一件事：树里没有的路径，不代表磁盘上没有
    ///
    /// 与 [`crate::SearchResult::truncated`] 同一条公理——`total_files` 不准
    /// ⇔ 本字段为真。为真时 `total_files` 是下界，不是准确值。
    ///
    /// # 它还是索引侧的「别删」信号
    ///
    /// `ki_ipc::search` 拿它为真就**一条都不删**（`service.rs` 的 `sync`）：
    /// 截断结果里缺失的路径只代表「没扫到」。所以改这个字段的语义必须同时跑
    /// `ki-ipc` 的同步用例，不只是 `ki-vault`。
    ///
    /// # 它不是「跳过清单被截断」
    ///
    /// 那是 [`ScanResult::skipped_truncated`]（ADR-013）：一个说树不全，
    /// 一个说清单只有前 50 条。两个布尔值，不许合并。
    pub truncated: bool,
    /// 被跳过的条目数。**精确**：所有原因之和，含那些不进清单的
    /// （[`ScanSkipReason::Internal`]）。
    ///
    /// # 它是「条目」不是「笔记」
    ///
    /// 目录、软链、FIFO、非 UTF-8 文件名都算一条。它与
    /// [`crate::SearchResult::skipped_files`]（索引侧的**笔记**数）是两件事，
    /// 任何时候不相等也合法——理由见 [`crate::skip`] 的模块文档。
    pub skipped: usize,
    /// 被跳过的条目清单，最多 `ki_vault::SKIP_LIST_MAX` 条。
    ///
    /// 诊断数据，不落盘：它描述的是**这一次**扫描看到了什么（ADR-013）。
    /// 空不等于 `skipped == 0`：`Internal` 类的条目只进总数、不进这里。
    pub skipped_detail: Vec<crate::skip::ScanSkipEntry>,
    /// 清单是否被截断（`skipped_detail` 只是前 N 条）。
    ///
    /// **只描述清单**，不影响 [`ScanResult::skipped`] 的精确性——契约照抄
    /// `EncodingStats`（ADR-012：total 精确、truncated 只描述清单）。
    /// 它也不能由 `skipped > skipped_detail.len()` 推出来：`Internal` 类的
    /// 条目本来就不在清单里，那样推会把「有 1 条 Internal」误报成「清单被截断」。
    pub skipped_truncated: bool,
    /// 本次扫描生效的忽略规则（内置名单 + 用户配置 + 配置错误）。
    ///
    /// # 它不是 `skipped` 的一部分
    ///
    /// 被忽略的条目**不在**这里，也不在 [`ScanResult::skipped`] 里：剪枝意味着
    /// 根本没访问过那棵子树，一个数都没有。忽略是「我按你的要求没看」，
    /// 跳过是「我试了，没成」——两件事，两个出口（ADR-013 / ADR-014）。
    pub ignore_rules: crate::ignore::IgnoreRules,
    /// 扫描耗时（毫秒），用于直接验证 M0 的 500ms 预算。
    pub elapsed_ms: u64,
}

/// 单篇笔记的读取结果。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NoteContent {
    /// Vault 相对路径（POSIX），与请求时传入的一致。
    pub path: String,
    /// 正文。BOM（UTF-8 / UTF-16 / UTF-32 的）已剥离。
    pub content: String,
    /// 这篇是从别的编码转过来的话，原编码的名字（`"GBK"` / `"Big5"` / `"UTF-16LE"`…）；
    /// `None` 严格等价于「磁盘上本来就是 UTF-8」。
    ///
    /// # 为什么是字符串而不是枚举
    ///
    /// 前端**只拿它拼一句话**（「已按 GBK 解码」），不据此分支——保存写回
    /// 哪个编码由 Rust 在写盘前重新探测，前端说了不算。枚举会逼前端再写一张
    /// 标签表，两张表迟早会漂；而一个「取值集合封闭但只用于展示」的字段，
    /// 字符串就是它最小的形状。
    ///
    /// 带 BOM 的 UTF-8 **不会**出现在这里：它是文件头的一种写法，不是
    /// 另一种字符集，提示用户「已按 UTF-8 (BOM) 解码」只会制造困惑。
    ///
    /// 不挂 `#[serde(default)]`：这个字段**永远**会被序列化（不是
    /// `skip_serializing_if`），所以线上形状恒为 `string | null`。
    /// 加了 `default` 反而会让生成出来的 TS 变成可选字段，前端就得多写
    /// 一次 `undefined` 判断——契约形状不该由 serde 的便利性决定。
    pub decoded_from: Option<String>,
    pub size: u64,
    pub mtime: i64,
}

/// 单篇笔记写入成功后的回执。
///
/// 刻意**不回传正文**：调用方刚把它发出去，回显一遍只是浪费一次 1MB 的序列化。
/// 但 `size` 与 `mtime` 必须是**写完之后的磁盘真值**，而不是"入参长度 + 现在时间"——
/// 前者能顺带证明落盘的字节数确实等于发出去的字节数（编码、换行转换出问题时会立刻不符）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    /// Vault 相对路径（POSIX），与请求时传入的一致。
    pub path: String,
    /// 写入后的文件字节数。
    pub size: u64,
    /// 写入后的 mtime（Unix 毫秒）。
    pub mtime: i64,
}

/// 自动保存的当前状态。
///
/// 每次 `note_edit` 都回传一份，前端据此渲染「已保存 / 未保存 / 出错」。
/// 把它做成结构体而不是让前端自己推断，是因为「有没有内容没落盘」这个事实
/// 只有持有待写内容的 Rust 侧知道——前端猜不出来。
///
/// 只 `Serialize` 不 `Deserialize`：它从 Rust 流向 UI，单向。
/// （`last_error` 里是 [`KiError`]，而错误类型本来就不该被反序列化成输入。）
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SaveState {
    /// 是否还有内容没落盘。
    pub dirty: bool,
    /// 最近一次成功落盘后的 mtime；本次会话从未落盘时为 `None`。
    pub last_saved_mtime: Option<i64>,
    /// 最近一次落盘的错误（典型是 [`KiError::Conflict`]）。落盘成功后清空。
    pub last_error: Option<KiError>,
}

/// `app_ping` 的返回值：IPC 通道的握手信息。
///
/// 这个结构体存在的唯一理由是**对称**。M0 时它返回 `serde_json::Value`，
/// 于是契约只有前端一侧——Rust 改了字段名，编译器不会拦，只有用户会看见
/// `undefined`。M1 要再加三个命令，再放任下去就是四个单边契约。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// 产品标识，恒为 `knowisle`（不是窗口标题，别拿它做展示）。
    pub name: String,
    /// 取自 `CARGO_PKG_VERSION`，与 `tauri.conf.json` 的 version 同源。
    pub version: String,
    /// 进程号。用来在"起了两个实例"时确认前端连的是哪一个。
    pub pid: u32,
}
