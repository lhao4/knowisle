//! 新建与重命名。
//!
//! 两个操作都**只碰目录项**，不写内容：新建落一个 0 字节文件，重命名搬一个
//! 目录项。内容写入是 [`crate::write`] 的事，两者刻意不共用代码——
//! 原子写的四步（`tmp → fsync → rename`）保护的是"旧内容不被截断"，
//! 而新建没有旧内容可保护，重命名搬的是整个文件（`fs::rename` 本身就是原子的）。
//!
//! # 新建为什么不用 `tmp → rename`
//!
//! 原子写的价值在于"目标文件在任何时刻都是完整的旧内容"。新建的目标**不存在**，
//! 没有旧内容可保护，`tmp → rename` 只会多出两个失效模式（tmp 撞名、tmp 残留）。
//! `create_new` 反而是更强的语义：它由内核原子地保证"这个名字此刻没人占"，
//! 而我们正需要这个——撞名必须失败，不能覆盖。
//!
//! # 重命名为啥要自己查一遍目标
//!
//! `fs::rename` 在 Unix 与 Windows 上都是**覆盖**语义（Windows 的
//! `MoveFileExW` 带 `MOVEFILE_REPLACE_EXISTING`）。直接调它等于把
//! "目标已存在"变成"目标被静默删掉"——这是数据丢失级的错误，而它不会报错。

use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use ki_core::KiError;

use crate::naming::normalize_note_name;
use crate::path::{resolve_for_write, resolve_in_vault};

/// 两步改名用的临时名前缀。
///
/// 必须是**扫描器不认的扩展名**（这里干脆没有扩展名），否则改名途中会在树里
/// 闪出一个幽灵节点；以点开头则保证它不会出现在任何正常排序里。
const RENAME_TMP_PREFIX: &str = ".knowisle.rename.";

/// 进程内单调序号：同一目录下并发改名（CLI + GUI）不会撞同一个临时名。
static RENAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// 在 `dir`（Vault 相对路径，空串表示根目录）下新建一个**空**笔记。
///
/// 返回最终落盘的 Vault 相对路径（POSIX 分隔符）。
///
/// # 为什么是 0 字节
///
/// 不写 `# 标题`、不写 BOM、不写任何占位符。用户没写的字，编辑器不该替他写——
/// 那会在第一篇笔记里就留下"这不是我打的"的内容，而撤销栈里还找不到它。
///
/// # 撞名
///
/// `create_new` 失败即 [`KiError::NameTaken`]，原文件一个字节都不动。
/// 这里没有 TOCTOU：存在性检查与创建是**同一个 syscall**（`O_CREAT|O_EXCL`）。
pub fn create_note(root: &Path, dir: &str, raw_name: &str) -> Result<String, KiError> {
    let name = normalize_note_name(raw_name)?;
    let rel = if dir.is_empty() {
        name
    } else {
        format!("{dir}/{name}")
    };

    // 父目录必须已存在、路径上不许有重解析点、目标不能是目录——
    // 三条都由 `resolve_for_write` 统一保证，与 `note_save` 是同一条边界。
    let target = resolve_for_write(root, &rel)?;

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|err| match err.kind() {
            ErrorKind::AlreadyExists => KiError::NameTaken { path: rel.clone() },
            _ => KiError::io(&rel, &err),
        })?;

    // 0 字节也要 fsync：不刷的话崩溃后可能留下"目录项在、文件不在"的中间态
    // （在部分文件系统上表现为一个无法打开的条目）。
    //
    // **不刷父目录**：空文件没有内容可丢，崩溃后最坏是"刚建的那篇不见了"，
    // 用户再建一次即可；而 Windows 上根本做不到目录 fsync（见 `write` 模块）。
    file.sync_all().map_err(|err| KiError::io(&rel, &err))?;
    drop(file);

    Ok(rel)
}

/// 把 `path` 改名为同一目录下的 `raw_name`，返回新的 Vault 相对路径。
///
/// # 只改名，不移动
///
/// 新名字只在**原目录内**生效。跨目录移动是另一个功能（拖拽 / 移动对话框），
/// 它要处理"目标目录不存在""跨设备 rename 失败""引用要不要跟着改"，
/// 不属于本轮范围。把新名字限定成单个文件名，这条边界就是结构性的，
/// 不依赖调用方自觉。
///
/// # 目标已存在
///
/// 返回 [`KiError::NameTaken`]，两个文件都不动。唯一的例外是"目标与源是同一个
/// 文件，只是大小写不同"——那走两步改名，见 [`move_no_clobber`]。
pub fn rename_note(root: &Path, path: &str, raw_name: &str) -> Result<String, KiError> {
    let new_name = normalize_note_name(raw_name)?;

    // 源必须存在，且路径上不许有重解析点。`resolve_in_vault` 顺带把源
    // 规范化成真实落点，Windows 上大小写也会被纠正成磁盘上的那一份。
    let source = resolve_in_vault(root, path)?;

    // 只改文件。目录改名会把一整片笔记从树里搬走（扫描器按路径建树，
    // 改名之后整棵子树都换了位置），而"移动目录"不在本轮范围内。
    // `fs::rename` 对目录是完全合法的，所以这道闸必须显式写出来。
    let meta = std::fs::metadata(&source).map_err(|err| KiError::io(path, &err))?;
    if !meta.is_file() {
        return Err(KiError::Io {
            path: path.to_owned(),
            message: "只能重命名文件，不能重命名目录".to_owned(),
        });
    }

    let parent = match path.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    };
    let new_rel = if parent.is_empty() {
        new_name
    } else {
        format!("{parent}/{new_name}")
    };

    // 同名改名：空操作。不查这一条的话会走两步改名，白白搬两次目录项，
    // 而且在"目标存在"的判据上制造一个自己跟自己撞名的特例。
    if new_rel == path {
        return Ok(new_rel);
    }

    let target = resolve_for_write(root, &new_rel)?;
    move_no_clobber(&source, &target, &new_rel)?;

    Ok(new_rel)
}

/// 把 `source` 搬到 `target`，**绝不覆盖**已存在的 `target`。
///
/// # 怎么区分"撞名"与"只改大小写"
///
/// 判据是**搬走之后再问一次目标还在不在**：
///
/// 1. 目标不存在 → 直接 `rename`。
/// 2. 目标存在 → 先把源挪到一个临时名，再看目标：
///    - 目标**不见了** → 它和源是同一个目录项（大小写不敏感的文件系统上
///      `foo.md` 与 `Foo.md` 共用一条记录），第二步把临时名改成目标名即可；
///    - 目标**还在** → 那是另一个文件，回滚并报 [`KiError::NameTaken`]。
///
/// # 为什么不用 inode / file index 比对
///
/// 那是"猜两个路径指向同一个文件"，而这里要回答的是更直接的问题：
/// "把源挪走之后，目标这个名字还在不在"。前者要平台专有 API（Windows 的
/// `file_index` 在 MSRV 1.82 上还没有稳定），而且**硬链接**会被判成同一个文件
/// ——于是"改名到一个已存在的硬链接"会静默成功，覆盖掉那条目录项。
/// 后者只用 `symlink_metadata`，三平台行为一致，顺带把硬链接也正确地拒掉。
///
/// # 崩溃窗口（已知代价）
///
/// 只有"目标已存在"这条分支有窗口：源已经被挪到 `.knowisle.rename.*`，
/// 而第二步还没执行。此刻崩溃，用户会看到一篇内容完好、名字是临时名的文件——
/// 数据没丢，但要手工改回来。这个窗口只在大小写改名时出现（普通改名是一步
/// `rename`，原子），而"rename 一个已存在的目标"本身就不是原子的，
/// 两步是能做到"不覆盖"的最小代价。
///
/// # TOCTOU
///
/// 检查与 rename 之间仍有时间窗：别的进程可以在这个窗口里创建目标。
/// 本地单用户应用接受这一点——能在这个窗口里动手的人，本来就有 Vault 的
/// 写权限，直接改笔记更省事。这是**有意识的取舍**，不是遗漏。
fn move_no_clobber(source: &Path, target: &Path, rel: &str) -> Result<(), KiError> {
    match std::fs::symlink_metadata(target) {
        Err(err) if err.kind() == ErrorKind::NotFound => {
            std::fs::rename(source, target).map_err(|err| KiError::io(rel, &err))
        }
        Err(err) => Err(KiError::io(rel, &err)),
        Ok(_) => {
            let parent = source.parent().ok_or_else(|| KiError::PathEscape {
                path: rel.to_owned(),
            })?;
            let temp = parent.join(format!(
                "{RENAME_TMP_PREFIX}{}.{}",
                std::process::id(),
                RENAME_SEQ.fetch_add(1, Ordering::Relaxed)
            ));

            std::fs::rename(source, &temp).map_err(|err| KiError::io(rel, &err))?;

            match std::fs::symlink_metadata(target) {
                // 目标跟着源一起消失了：同一个目录项，只改大小写。
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    match std::fs::rename(&temp, target) {
                        Ok(()) => Ok(()),
                        Err(err) => {
                            // 第二步失败：把源放回原处。放不回去也不能再抛一个新错误
                            // 把原因盖掉——临时名写在错误里，用户至少找得到文件。
                            let _ = std::fs::rename(&temp, source);
                            Err(KiError::io(rel, &err))
                        }
                    }
                }
                // 另一个文件：回滚，报撞名。
                Ok(_) => {
                    let _ = std::fs::rename(&temp, source);
                    Err(KiError::NameTaken {
                        path: rel.to_owned(),
                    })
                }
                Err(err) => {
                    let _ = std::fs::rename(&temp, source);
                    Err(KiError::io(rel, &err))
                }
            }
        }
    }
}
