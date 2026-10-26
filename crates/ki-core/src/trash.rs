//! 回收站的领域类型。
//!
//! # 为什么这几个结构体在 `ki-core` 而不是 `ki-vault`
//!
//! 它们跨 IPC 出去，前端要拿它们渲染列表与提示。放 `ki-vault` 的话，
//! `ki-ipc` 得为它们写一份镜像 DTO——那正是 `tauri-specta` 要消灭的东西
//! （同 [`crate::node::FileNode`] 的理由）。它们没有 IO，也就没有违反 A2。
//!
//! # 返回的是**事实**，不是文案
//!
//! 恢复之后用户要知道三件互相独立的事：
//!
//! 1. 它落在哪（`path`），以及本来想落在哪（`requested`）；
//! 2. 为什么没落在原处（`fallback`），以及是不是撞名改了名（`renamed`）；
//! 3. 搜不搜得到（`indexed`）。
//!
//! 三件事**可以同时发生**：原目录没了 + 根目录已有同名文件 + 索引库坏了，
//! 一次恢复里全都能撞上。所以它们是并列的字段，而不是一句拼好的提示语——
//! 拼好的那句话只能表达其中一种，而 UI 必须把三种都说出来。
//!
//! 列表同理：[`TrashEntry`] 只回答「回收目录里有什么、知不知道它原来在哪」，
//! 不回答「该显示成什么」。`origin` 为 `None` 时那句「原路径未知」是 UI 的话，
//! 不是这一层的话。

use serde::{Deserialize, Serialize};
use specta::Type;

/// 回收目录里的一条。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TrashEntry {
    /// 回收目录里的文件名（`<unix_ms>-<原名>`）。恢复时用它定位。
    ///
    /// 它是**磁盘上的真名**，不是 `origin` 的末段：两者在正常情况下一致，
    /// 但清单缺失时只剩它，而恢复必须能靠它把文件搬回去。
    pub entry: String,
    /// 展示用的原名（`c.md`）。
    ///
    /// 由 Rust 算：有原路径就取它的末段，没有就从 `entry` 里剥掉时间戳前缀。
    /// **不让前端各写一遍**——`<unix_ms>-` 的剥法一旦两边不一致，用户会看到
    /// 一个"名字和恢复出来的不一样"的列表，而且没有任何报错。
    pub name: String,
    /// 删除前它在 Vault 里的相对路径（POSIX）。
    ///
    /// `None` = 清单里没有这条记录，或者记录损坏/不可信。**不是错误**：
    /// 文件还在，只是不知道它原来在哪。前端必须把它显示成「原路径未知」，
    /// 恢复时落到库根目录并明确告知。
    pub origin: Option<String>,
    /// 删除时间（Unix 毫秒）。
    ///
    /// 三级来源：清单 → 文件名前缀 → 文件 mtime。所以它**恒有值**，
    /// 前端不需要为 `null` 写一条分支。
    pub deleted_at: i64,
    /// 字节数（磁盘真值）。
    pub size: u64,
}

/// 恢复没回到「原处」的原因。
///
/// # 为什么不是 `degraded: bool`
///
/// 三种成因对用户意味着**三个不同的下一步**：
///
/// | 值 | 磁盘上的事实 | 用户该做什么 |
/// |---|---|---|
/// | [`RecordMissing`] | 清单里没有这条记录 | 什么都不用做，文件已经回来了 |
/// | [`RecordCorrupt`] | 记录在，但读不出来 | 同上；若在意原名，去 `.knowisle/trash-manifest/` 看看 |
/// | [`OriginGone`] | 记录完好，**原目录**没了 | 目录是他自己删的，别怀疑回收站坏了 |
///
/// 压成一个布尔时，前端只剩一句「原路径记录缺失」可用——对第三种情况，
/// 这句话在**指错方向**：记录没丢，是目录没了。
///
/// [`RecordMissing`]: Fallback::RecordMissing
/// [`RecordCorrupt`]: Fallback::RecordCorrupt
/// [`OriginGone`]: Fallback::OriginGone
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum Fallback {
    /// 清单里**没有**这条记录。
    ///
    /// 两种来路：删除流程在「搬完文件、还没写清单」的窗口里崩过；或者清单
    /// 文件被外部删掉了。两者在磁盘上是同一个状态。
    RecordMissing,
    /// 清单里有这条记录，但**读不出来**。
    ///
    /// JSON 半截、字段缺失、`origin` 被改成越界路径，都落到这里。
    /// 记录文件本身还在，只是不能用——这与「没有记录」是两件事。
    RecordCorrupt,
    /// 记录完好，**原目录已经不存在了**（外部删除）。
    ///
    /// 刻意**不代建**目录：用户删掉那个目录可能正是他的意思，恢复一篇笔记
    /// 不该把一整片目录树复活（ADR-008）。
    OriginGone,
}

/// 一次恢复的结果。
///
/// # 字段的分工：`requested` 是意图，`path` 是事实
///
/// 这两个字段在**降级**时会分叉，而分叉正是它们存在的理由：
///
/// | 场景 | `requested`（想回哪） | `path`（实际落在哪） |
/// |---|---|---|
/// | 正常 | `a/b/c.md` | `a/b/c.md` |
/// | 撞名 | `a/b/c.md` | `a/b/c 2.md` |
/// | 原目录没了 | `a/b/c.md` | `c.md` |
/// | 清单丢了 | `c.md` | `c.md`（或 `c 2.md`） |
///
/// 所以**不能**用 `path != requested` 判断有没有改名：`OriginGone` 下两者
/// 天然不同，那样写会把"降级"说成"改名"。`renamed` 由真正执行改名的那一步
/// （`unique_target`）置位。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    /// 恢复后它在 Vault 里的相对路径（POSIX）。**事实**。
    pub path: String,
    /// 本来想回到哪。**意图**。
    ///
    /// 已知原路径时它就是 `origin`（哪怕那个目录已经不在了）；清单不可用时
    /// 才是「库根目录 + 从文件名里剥出来的原名」。
    ///
    /// 它与 `path` 不同**不一定**意味着改名——见类型文档的表格。
    pub requested: String,
    /// 为什么没回到 `requested`。`None` = 原路回到了意图路径（可能改了名）。
    pub fallback: Option<Fallback>,
    /// 因为目标已存在而**真的**改了名（`c.md` → `c 2.md`）。两条都在，谁也没被覆盖。
    ///
    /// 与 `fallback` 正交：`OriginGone` 落到库根时，根目录也可能已有同名文件。
    pub renamed: bool,
    /// 恢复之后索引里有没有这一行。
    ///
    /// `false` **不是失败**：文件已经在正确的位置，判成失败只会诱导用户重试
    /// （而重试会得到 `c 2.md`）。索引库可整库重建（ADR-004），所以前端该说的是
    /// 「索引未更新，重启应用会重建」。
    ///
    /// `ki-vault` 不索引，它返回时恒为 `false`；`trash_restore` 补完索引再置真。
    pub indexed: bool,
}

/// 一次清空的结果。
///
/// # 为什么不是 `Result<u64, KiError>`
///
/// 一个文件被别的程序占住时，其余条目照删。旧签名在这种情况下返回第一个
/// `Err` 并**丢掉 `removed`**——前端拿到错误就 return，既不刷新列表也不说
/// 删了几个，于是面板上还列着已经被删掉的文件。
///
/// 清空是「能删的都删掉」的意图，部分成功必须能被表达出来。所以它**永远
/// `Ok`**（除非回收目录本身读不出来——那时一个都没动，是真的失败）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PurgeResult {
    /// 真的从磁盘上抹掉了多少个文件。
    pub removed: u64,
    /// 没能删掉的个数。它们**仍然在回收目录里**，所以列表刷新后还看得见。
    ///
    /// 典型成因是被别的程序占用（Windows 上是共享冲突，Unix 上是只读父目录）。
    pub failed: u64,
}
