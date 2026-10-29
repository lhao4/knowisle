//! 批量转存：把「待转存清单」一次处理完。
//!
//! # 它**不是**第二条转存实现
//!
//! 每一篇都调 [`ki_vault::transcode_to_utf8`]——同一份「备份 → 原子写 → 重读」。
//! 本模块只负责三件转存本身不该关心的事：
//!
//! 1. **失败不中断**：一篇失败记下来继续跑（ADR-012 的失败模式表）；
//! 2. **可取消**：1000 篇要跑十几秒，用户必须能停；
//! 3. **真实进度**：逐篇回报，不是「发了几个请求」。
//!
//! # 为什么取消是「跑完当前这一篇」
//!
//! 转存是「备份 + 原子写」，中途打断会留下一次写了一半的状态——虽然
//! `atomic_replace` 保证不会出现半截文件，但**备份已写、目标没写**的中间态
//! 会让 `history/` 里多一份永远不会被用到的副本。取消的粒度定在「篇与篇
//! 之间」，代价是最多晚一篇停下（≤20ms），换来的是每一次取消都落在
//! 一个干净的状态上。
//!
//! # 已转好的不回滚
//!
//! 取消、甚至部分失败，都不撤销已经改写成 UTF-8 的那几篇。它们本来就是
//! 用户要的结果，`history/` 里也有备份。回滚需要「反向转存」，而那要
//! 重新猜原始编码——转存刻意丢掉了那个信息（目标是 UTF-8 无 BOM）。
//!
//! # 进度为什么是「可查的状态」而不是「事件」
//!
//! 事件（`app.emit`）要 `AppHandle`，而本 crate 关掉了 `wry`，命令只能写成
//! `fn<R: Runtime>(app: AppHandle<R>)`——`tauri_specta::collect_commands!`
//! 会在 `ipc_builder<R>` 里遇到「内层函数重复声明 R」而编译失败。
//!
//! 换成「Rust 侧存一份真实进度 + 前端按 token 查」：进度仍然来自这里
//! （每跑完一篇写一次），前端拿到的不是「发了几个请求」的猜测。代价是
//! 批量期间每 120ms 一次 IPC——那是一次互斥锁读 + 一次序列化，比一篇的
//! 转存便宜两个数量级。

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use ki_core::{TranscodeBatch, TranscodeFailure, TranscodeProgress};
use ki_vault::transcode_to_utf8;

use crate::{autosave, search};

/// 已被要求取消的批次 token。
///
/// # 为什么 token 由前端给
///
/// 取消命令必须先于「知道 token」到达。若 token 由 Rust 生成并作为返回值
/// 给出，那么从「用户点开始」到「拿到 token」之间有一个窗口，点取消时
/// 前端手里还没有 token，取消就丢了。前端在**发起批量之前**生成 token
/// （一个递增整数），两边从第一毫秒起就对得上。
static CANCELLED: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

/// 正在跑的批次的真实进度。跑完即移除。
static PROGRESS: Mutex<Vec<TranscodeProgress>> = Mutex::new(Vec::new());

fn cancelled_set() -> std::sync::MutexGuard<'static, Option<HashSet<u64>>> {
    CANCELLED.lock().unwrap_or_else(PoisonError::into_inner)
}

fn progress_slot() -> std::sync::MutexGuard<'static, Vec<TranscodeProgress>> {
    PROGRESS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 请求取消某一批。幂等：重复请求、请求一个不存在的 token 都是空操作。
pub fn cancel(token: u64) {
    let mut guard = cancelled_set();
    guard.get_or_insert_with(HashSet::new).insert(token);
}

fn is_cancelled(token: u64) -> bool {
    cancelled_set()
        .as_ref()
        .is_some_and(|set| set.contains(&token))
}

/// 查某一批的当前进度。没有这一批（还没开始 / 已经结束）时返回 `None`。
///
/// 前端把 `None` 读成「没有更新的进度」，**不许**读成「进度归零」：
/// 批量的最终事实由 `note_transcode_batch` 的返回值给出，进度只服务
/// 「正在跑」的那一段。
pub fn progress(token: u64) -> Option<TranscodeProgress> {
    progress_slot()
        .iter()
        .find(|entry| entry.token == token)
        .cloned()
}

fn publish(progress: &TranscodeProgress) {
    let mut list = progress_slot();
    match list.iter_mut().find(|entry| entry.token == progress.token) {
        Some(slot) => *slot = progress.clone(),
        None => list.push(progress.clone()),
    }
}

/// 收尾时清掉自己的 token 与进度。
///
/// 不清 token 的话，同一个 token 被复用（前端重启后从头计数）会让新批次
/// 一启动就自杀。前端不复用 token 是它的事，这里不该依赖它。
fn forget(token: u64) {
    if let Some(set) = cancelled_set().as_mut() {
        set.remove(&token);
    }
    progress_slot().retain(|entry| entry.token != token);
}

/// 逐篇转存，**失败不中断**。
///
/// 每跑完一篇写一次进度（含失败）。写的是「已经处理完」的事实，不是估算——
/// 前端据此画的进度条不会在最后一篇跳一下。
///
/// # `expected_mtime` 为什么是 `None`
///
/// 单篇入口传 `Some(mtime)` 是为了「用户在编辑器里打开着这一篇，别把别人
/// 刚写的东西盖掉」。批量没有这个前提：清单来自索引，用户没有正在编辑
/// 其中任何一篇（前端在开始前会先 flush 当前那篇）。传 `None` 的语义是
/// 「按磁盘现状转」——而 `transcode_to_utf8` 会**重新读一遍当前字节**再解码，
/// 所以外部刚改过的内容不会丢，只是编码被规范化了。备份也是那一版字节。
pub fn run_batch(root: &Path, paths: Vec<String>, token: u64) -> TranscodeBatch {
    let total = paths.len();
    let mut out = TranscodeBatch::empty();

    for (index, path) in paths.into_iter().enumerate() {
        if is_cancelled(token) {
            out.cancelled = true;
            break;
        }

        match transcode_to_utf8(root, &path, None) {
            Ok(note) => {
                // 槽位对齐：这一篇可能正开在编辑器里。不做这一步，磁盘换了
                // mtime 而基线没换，下一次自动保存必然假 `CONFLICT`。
                autosave::rebase_after_transcode(root, &path, &note);
                // 索引行**不手改**：走正常重索引路径，`src_encoding` 由
                // `indexed_note` 从「刚读到的这篇现在是 UTF-8」重新算成 NULL。
                // 失败不改写本次结果：文件已经转好了，索引晚一拍对齐是
                // 下一次同步的事（`vault_encoding_stats` 会多显示一篇，无害）。
                let _ = search::reindex(root, &path);
                out.ok.push(path.clone());
            }
            Err(error) => out.failed.push(TranscodeFailure {
                path: path.clone(),
                error,
            }),
        }

        publish(&TranscodeProgress {
            token,
            done: index + 1,
            total,
            path,
            failed: out.failed.len(),
        });
    }

    forget(token);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取消是幂等的，且不影响别的 token。
    ///
    /// 这条用例守的是「取消一个不存在的 token」这个真实场景：用户点了取消，
    /// 批次刚好自己跑完了（`forget` 已经清掉），此时取消命令仍会到达。
    #[test]
    fn cancel_is_idempotent_and_scoped_to_its_token() {
        cancel(7);
        cancel(7);
        assert!(is_cancelled(7));
        assert!(!is_cancelled(8));
        forget(7);
        assert!(!is_cancelled(7));
        // 清一个已经不存在的 token 不该 panic。
        forget(7);
    }
}
