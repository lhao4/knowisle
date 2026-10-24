//! 原子写。
//!
//! 公理 A5 说「所有写操作必须 `tmp → fsync → rename`，崩溃后可自愈」。
//! 本模块就是这条公理的唯一实现，也是 M1 唯一不可妥协的验收点所在。
//!
//! # 为什么必须是这四步
//!
//! 直接 `fs::write(path, content)` 有两个独立的致命窗口：
//!
//! 1. **截断窗口**：`write` 会先 `O_TRUNC`。此刻崩溃 = 文件变成 0 字节，
//!    旧内容和新内容同时消失。这是最常见也最难解释的数据丢失。
//! 2. **半截窗口**：即使不截断，写 1MB 要多次 `write(2)`，中途崩溃留下
//!    前 N 字节的新内容 + 后 M 字节的旧内容。这种"缝合怪"比空文件更坏，
//!    因为它看起来像一篇正常笔记。
//!
//! 四步各自堵一个洞：
//!
//! | 步骤 | 堵的洞 |
//! |---|---|
//! | 写 tmp（`create_new`） | 不碰目标文件，目标在任何时刻都是完整的旧内容 |
//! | `sync_all()` | 不 fsync 就 rename，崩溃后可能得到"有目录项、没数据块"的空文件 |
//! | `rename` | 同一文件系统内是原子替换，目标要么是旧内容、要么是新内容 |
//! | fsync 父目录 | 不刷目录，rename 本身可能在崩溃后丢失（回退成旧内容，不是损坏） |
//!
//! 注意最后一步的语义：**丢失 rename 是允许的**（结果仍是旧内容），
//! 我们要杜绝的只有"半截"。所以 Windows 上做不到目录 fsync 时，
//! 退化结果是"可能回到旧内容"，而不是"可能损坏"——这是可接受的降级。
//!
//! # tmp 文件为什么放在目标同目录
//!
//! `rename` 只在**同一文件系统内**是原子的。放 `%TEMP%` 或 `/tmp` 上，
//! 跨设备时 `rename` 会返回 `EXDEV`，只能退化成"复制 + 删除"——原子性全没了。
//! 所以 tmp 必须和目标同一个目录，代价是崩溃后会留下孤儿 tmp 文件（见 BACKLOG）。
//!
//! # 命名
//!
//! `.knowisle.tmp.<pid>.<seq>`：以点开头（隐藏）、扩展名不是 `.md`
//! （扫描器只认 `.md` / `.markdown`，所以不会出现在侧栏），带 pid 与进程内
//! 单调序号（并发保存同一目录时不互相覆盖）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use ki_core::{KiError, SaveResult};

use crate::path::resolve_for_write;
use crate::scan::{stat, MAX_NOTE_BYTES};

/// 临时文件前缀。改名时扫描器靠扩展名过滤，不靠这个前缀——它只是给人看的。
const TMP_PREFIX: &str = ".knowisle.tmp.";

/// 进程内单调序号，保证同一目录下并发保存不会撞名。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 原子地把 `content` 写入 Vault 内的 `rel`，返回落盘后的真实大小与 mtime。
///
/// 无条件覆盖：不检查磁盘上的现状。要防"覆盖掉别人刚做的修改"，
/// 用 [`write_note_checked`]。
///
/// # 边界
///
/// 路径校验全部交给 [`resolve_for_write`]：父目录必须在库内且**已存在**
/// （本函数不代建目录——那属于上层的产品决策），最终组件不能是重解析点。
///
/// # 内容
///
/// 正文**逐字节原样**写入，不做换行转换、不加末尾换行、不写 BOM。
/// 注意 `read_note` 会剥掉开头的 UTF-8 BOM，所以调用方不该自己塞一个进去，
/// 否则"写进去的"和"读出来的"会差三个字节。
///
/// # 上限
///
/// 与 [`crate::read_note`] 共用 [`MAX_NOTE_BYTES`]：写不进去的内容，
/// 也就不该被写进去——能写不能读是一种最难排查的不一致。
pub fn write_note(root: &Path, rel: &str, content: &str) -> Result<SaveResult, KiError> {
    write_note_checked(root, rel, content, None)
}

/// 带乐观锁的原子写：只有磁盘上的 mtime 仍等于 `expected_mtime` 时才写。
///
/// `expected_mtime` 为 `None` 时等价于 [`write_note`]（无条件覆盖）。
///
/// # 为什么需要它
///
/// 自动保存会在用户没有明确动作的情况下写盘。如果用户在 Obsidian 里改了同一篇，
/// 知屿这边 1.5 秒后把它覆盖掉——丢的是**别人已经保存好的数据**，
/// 比断电丢自己未保存的输入严重一档。断电只是丢了"还没写的"，
/// 这个是抹掉了"已经写好的"。
///
/// # 语义边界
///
/// - 检查发生在写盘**之前**：抛出 [`KiError::Conflict`] 时文件一个字节都没动。
/// - 只比 mtime，不比内容 hash：算 hash 要把文件整个读一遍，而自动保存每 1.5 秒
///   触发一次，1MB 的笔记就是每 1.5 秒多读 1MB。mtime 是行业惯例（Obsidian 同款）。
/// - **已知漏检**：mtime 精度只有 1 秒的文件系统（FAT32、部分网络盘）上，
///   同一秒内的两次外部修改可能拿到相同 mtime。这是取舍，不是疏忽；
///   真要补得加 size + 内容抽样，收益与成本不成比例。
pub fn write_note_checked(
    root: &Path,
    rel: &str,
    content: &str,
    expected_mtime: Option<i64>,
) -> Result<SaveResult, KiError> {
    let bytes = content.as_bytes();
    if bytes.len() as u64 > MAX_NOTE_BYTES {
        return Err(KiError::TooLarge {
            path: rel.to_owned(),
            size: bytes.len() as u64,
        });
    }

    let target = resolve_for_write(root, rel)?;

    // 冲突检查紧挨着 commit，把 TOCTOU 窗口压到最小。
    // 文件不存在时 `metadata` 失败，`disk_mtime` 为 None —— 那也属于冲突：
    // 调用方读到过的文件现在没了，不能当作"新建"静默写出去。
    if let Some(expected) = expected_mtime {
        let disk_mtime = std::fs::metadata(&target)
            .ok()
            .map(|meta| stat(Some(meta)).1);
        if disk_mtime != Some(expected) {
            return Err(KiError::Conflict {
                path: rel.to_owned(),
                expected_mtime: expected,
                disk_mtime,
            });
        }
    }

    let parent = target.parent().ok_or_else(|| KiError::PathEscape {
        path: rel.to_owned(),
    })?;

    let tmp = parent.join(format!(
        "{TMP_PREFIX}{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    commit(&tmp, &target, bytes).map_err(|e| KiError::io(rel, &e))?;

    // 回执取磁盘真值，不拿入参长度充数——编码或换行被改写时这里会立刻不符。
    let meta = std::fs::metadata(&target).map_err(|e| KiError::io(rel, &e))?;
    let (size, mtime) = stat(Some(meta));

    Ok(SaveResult {
        path: rel.to_owned(),
        size,
        mtime,
    })
}

/// 四步落地。任何一步失败都清掉 tmp，绝不留下半成品。
///
/// 抽出来是为了让错误路径只有一个出口：`write_note` 里若用 `?` 分散写四步，
/// 第三步失败时前两步的 tmp 就没人删了。
fn commit(tmp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    match write_then_swap(tmp, target, bytes) {
        Ok(()) => Ok(()),
        Err(err) => {
            // 清理失败不重要：留下的孤儿 tmp 不会被当成笔记，也不影响下次写入。
            // 真正的错误要原样返回，不能被清理的次生错误盖掉。
            let _ = std::fs::remove_file(tmp);
            Err(err)
        }
    }
}

fn write_then_swap(tmp: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // `create_new` 而不是 `create`：撞名时宁可报错，也不要复用别人（或上次崩溃）
    // 留下的 tmp —— 复用一个长度更长的旧 tmp 会让本次写入带上尾巴。
    let mut file = OpenOptions::new().write(true).create_new(true).open(tmp)?;

    file.write_all(bytes)?;
    // 只 fsync 不够：`write_all` 之后数据可能还在内核页缓存里，
    // 此时 rename 成功而崩溃，会得到"有目录项、内容为空"的文件。
    file.sync_all()?;
    // Windows 上重命名一个仍打开的文件会失败，必须先显式关闭。
    drop(file);

    std::fs::rename(tmp, target)?;
    sync_parent_dir(target)?;

    Ok(())
}

/// 刷父目录，让 rename 这条目录项变更本身落盘。
///
/// Windows 上 `std::fs` 打不开目录（没有 `FILE_FLAG_BACKUP_SEMANTICS`），
/// 因此这里是空实现。降级后果是"崩溃后 rename 可能没生效"——结果是旧内容，
/// 不是半截内容，所以不破坏本模块的核心承诺。要补的话得走 `windows-sys`
/// 手工 `CreateFileW` + `FlushFileBuffers`，M1 阶段不值得。
#[cfg(unix)]
fn sync_parent_dir(target: &Path) -> std::io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "目标路径没有父目录")
    })?;
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_target: &Path) -> std::io::Result<()> {
    Ok(())
}
