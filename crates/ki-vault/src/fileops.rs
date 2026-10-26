//! 新建、重命名与删除。
//!
//! 三个操作都**只碰目录项**，不写内容：新建落一个 0 字节文件，重命名搬一个
//! 目录项，删除把它搬进回收目录。内容写入是 [`crate::write`] 的事，两者刻意
//! 不共用代码——原子写的四步（`tmp → fsync → rename`）保护的是"旧内容不被截断"，
//! 而新建没有旧内容可保护，重命名与删除搬的是整个文件（`fs::rename` 本身就是原子的）。
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
use crate::path::{reject_metadata_path, resolve_for_write, resolve_in_vault, to_posix};
use crate::trash::{record as record_in_manifest, TRASH_DIR};

/// 两步改名用的临时名前缀。
///
/// 必须是**扫描器不认的扩展名**（这里干脆没有扩展名），否则改名途中会在树里
/// 闪出一个幽灵节点；以点开头则保证它不会出现在任何正常排序里。
const RENAME_TMP_PREFIX: &str = ".knowisle.rename.";

/// 进程内单调序号：同一目录下并发改名（CLI + GUI）不会撞同一个临时名。
static RENAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// 同一毫秒内的撞名重试上限。
///
/// 不是业务参数，是**收敛保证**：`while 存在就换名字` 的循环必须有上界，
/// 否则一个被塞满同名文件的回收目录会让删除永久卡住（而它看起来像"卡死"，
/// 不像"出错"）。1000 个同毫秒同名文件在任何真实场景下都不可能出现。
const TRASH_SEQ_LIMIT: u32 = 1000;

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
    // `.knowisle/` 下的东西不是用户内容，改名会让索引 / 回收目录里的路径失联。
    // 这条闸与 `delete_note` 共用，见 `path::reject_metadata_path`。
    reject_metadata_path(path)?;

    let new_name = normalize_note_name(raw_name)?;

    // 源必须存在，且路径上不许有重解析点。`resolve_in_vault` 顺带把源
    // 规范化成真实落点，Windows 上大小写也会被纠正成磁盘上的那一份。
    let source = resolve_in_vault(root, path)?;

    // 只改文件。目录改名会把一整片笔记从树里搬走（扫描器按路径建树，
    // 改名之后整棵子树都换了位置），而"移动目录"不在本轮范围内。
    // `fs::rename` 对目录是完全合法的，所以这道闸必须显式写出来。
    let meta = std::fs::metadata(&source).map_err(|err| KiError::io(path, &err))?;
    if !meta.is_file() {
        return Err(KiError::NotAFile {
            path: path.to_owned(),
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

/// 把 `path` 移进 `.knowisle/trash/`，返回它在回收目录里的 Vault 相对路径。
///
/// # 为什么不是 `fs::remove_file`
///
/// 删除是对公理 A1（磁盘上的 Markdown 是唯一真相源）**最直接的违背**：
/// 库里那份文件一消失，磁盘上就再没有它的任何痕迹。local-first 产品第一次
/// 误删就失去信任，而"移进一个目录"与"抹掉 inode"之间只差一次 `rename`——
/// 同盘原子、零新依赖（`trash` crate 会带平台特定依赖，违反红线）。
///
/// 目标名是 `<unix_ms>-<原名>`：时间戳保证两次删除不会互相覆盖，原名保证
/// 用户打开 `.knowisle/trash/` 还认得出哪个是哪个。
///
/// # 原路径记在哪
///
/// 文件名里只有原名，`a/b/c.md` 与 `x/c.md` 删掉之后长得一模一样。所以搬完
/// 之后还要往清单里写一条 `entry → origin`（[`crate::trash`]，ADR-008）。
///
/// **顺序是先搬文件、后写清单**，不能反：
///
/// - 先搬后写，崩溃只丢"它原来在哪"——文件还在回收站里，能恢复到库根目录。
/// - 先写后搬，崩溃会留下"清单说有、文件没有"的悬空条目，列表里多一个
///   点开就报错的假记录。
///
/// 清单写失败**不让删除失败**：文件已经安全了，这时报错只会让用户以为
/// "没删掉"再去删一次，而第二次会报"文件不存在"。降级方向是明确的：
/// 列表里那一条显示成「原路径未知」。
///
/// # 只删文件
///
/// `meta.is_file()` 是必须的：`fs::rename` 对目录完全合法，一次能搬走一整片
/// 笔记，而"删除目录"不在本轮范围内（与 [`rename_note`] 同一条闸）。
///
/// # 失败时的状态
///
/// - 建不出回收目录 → 报错，**绝不降级成永久删除**。静默降级是这条路径上
///   唯一能造成真实数据丢失的写法。
/// - `fs::rename` 失败（跨设备、目标被占用）→ 文件仍在原位，一个字节没动，
///   直接重试即可。
///
/// # 崩溃窗口
///
/// 文件侧只有一次 `rename`，它是原子的：崩溃后要么文件还在原位（删除没发生），
/// 要么已经在 trash 里。清单侧是一次 `tmp → fsync → rename`（A5），
/// 所以它要么不存在、要么完整。不刷父目录——Windows 上根本做不到目录 fsync
/// （见 `write` 模块），而"删除没生效"是安全方向的失败。
pub fn delete_note(root: &Path, path: &str) -> Result<String, KiError> {
    // 闸 1：`.knowisle/` 下的东西一律拒绝。必须在 `resolve_in_vault` 之前——
    // 那条路对"文件不存在"给的是 OS 的 ENOENT 文案，用户看不懂它为什么被拒。
    reject_metadata_path(path)?;

    // 闸 2：语法 + 重解析点 + 必须存在（`resolve_in_vault` 的职责）。
    // 返回的是规范化后的真实落点，Windows 上大小写也被纠正成磁盘上的那一份。
    let source = resolve_in_vault(root, path)?;

    // 闸 3：只删文件。理由见上面的「只删文件」。
    let meta = std::fs::metadata(&source).map_err(|err| KiError::io(path, &err))?;
    if !meta.is_file() {
        return Err(KiError::NotAFile {
            path: path.to_owned(),
        });
    }

    // 只取 `file_name()`：目标名里绝不允许出现路径分隔符。取规范化路径而不是
    // 原始 `path` 的末段，是因为前者已经是磁盘上的真名（大小写也一致）。
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| KiError::InvalidEncoding {
            path: path.to_owned(),
        })?;

    let canonical_root = dunce::canonicalize(root)
        .map_err(|err| KiError::io(root.to_string_lossy().into_owned(), &err))?;
    let trash = canonical_root.join(TRASH_DIR);

    // 建不出来就报错返回：此时文件还在原位，用户只是删不掉，而不是丢了。
    // 这句「不会降级为永久删除」是这条路径上唯一能防真实数据丢失的话，不许省。
    std::fs::create_dir_all(&trash).map_err(|err| {
        KiError::io_msg(
            TRASH_DIR,
            &err,
            "无法创建回收目录（已取消删除，不会降级为永久删除）",
        )
    })?;

    // `.knowisle` 本身可能是软链 / junction。`create_dir_all` 会顺着它建到库外，
    // 而 `fs::rename` 于是把笔记搬出 Vault——用户以为删了，其实文件到了别处。
    // 规范化后比一次是最便宜的挡法（与 `resolve_for_write` 的父目录检查同一手法）。
    let trash_real = dunce::canonicalize(&trash).map_err(|err| KiError::io(TRASH_DIR, &err))?;
    if !trash_real.starts_with(&canonical_root) {
        return Err(KiError::PathEscape {
            path: TRASH_DIR.to_owned(),
        });
    }

    let stamp = unix_ms();
    // `symlink_metadata` 而不是 `exists`：回收目录里残留一个同名软链时，
    // `exists` 会跟随它去问目标，而这里要问的是"这个名字有没有人占"。
    let candidate = unique_trash_name(stamp, name, |candidate| {
        std::fs::symlink_metadata(trash_real.join(candidate)).is_ok()
    })
    .ok_or_else(|| {
        KiError::Internal(format!(
            "回收目录里已有 {TRASH_SEQ_LIMIT} 个同毫秒同名的文件，已放弃删除：{path}"
        ))
    })?;

    std::fs::rename(&source, trash_real.join(&candidate)).map_err(|err| KiError::io(path, &err))?;

    // 文件已经安全了，才写清单。原路径取**规范化后的相对路径**而不是入参：
    // 入参可能是 `./notes/a.md` 或大小写不对的写法，而恢复要按磁盘上的真名
    // 落回去（Windows 上大小写会被 `resolve_in_vault` 纠正，那正是我们要的）。
    let origin = source
        .strip_prefix(&canonical_root)
        .ok()
        .and_then(to_posix)
        .unwrap_or_else(|| path.to_owned());
    if let Err(err) = record_in_manifest(&canonical_root, &candidate, &origin, stamp) {
        // 不返回错误：删除已经成功，报错会引导用户重试一次注定失败的操作。
        eprintln!("回收站清单写入失败（文件已在回收站，列表会退化成只显示文件名）：{err}");
    }

    Ok(format!("{TRASH_DIR}/{candidate}"))
}

/// 在 `<unix_ms>-<原名>` 被占用时依次尝试 `<unix_ms>-<seq>-<原名>`。
///
/// 抽成纯函数（`taken` 由调用方注入）是为了让"同一毫秒撞名"这条分支能被
/// **确定性**地测到：真实删除两次之间必然跨过一毫秒，端到端测试永远走不到
/// 撞名分支，而它恰恰是唯一会覆盖别人文件的路径。
///
/// 序号插在时间戳与原名之间，而不是追加到末尾：`a.md-1` 会让文件看起来不再
/// 是 Markdown，而 `1699-1-a.md` 仍然一眼认得出原名。
///
/// 返回 `None` 表示重试次数用尽（见 [`TRASH_SEQ_LIMIT`]）。
fn unique_trash_name(stamp: i64, name: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let mut candidate = format!("{stamp}-{name}");
    let mut seq = 0u32;
    while taken(&candidate) {
        seq += 1;
        if seq > TRASH_SEQ_LIMIT {
            return None;
        }
        candidate = format!("{stamp}-{seq}-{name}");
    }
    Some(candidate)
}

/// 当前 Unix 毫秒时间戳。取不到（时钟早于 epoch）时退化为 0——
/// 那样只会让两次删除撞名，而撞名有 `-<seq>` 兜底，不会丢文件。
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
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
///
/// 恢复（[`crate::trash::restore`]）共用它：两边都是"搬一个目录项，绝不覆盖"。
pub(crate) fn move_no_clobber(source: &Path, target: &Path, rel: &str) -> Result<(), KiError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 空回收目录：第一个候选名就是 `<stamp>-<原名>`，且原名（含扩展名）完整。
    #[test]
    fn the_first_trash_name_is_stamp_plus_the_original_name() {
        let name = unique_trash_name(1_700_000_000_000, "a.md", |_| false).unwrap();
        assert_eq!(name, "1700000000000-a.md");
    }

    /// 撞名一次 → `-1`，且扩展名仍在末尾。
    ///
    /// 这条分支在真实删除里几乎撞不到（两次删除必然跨毫秒），所以它只能
    /// 靠注入 `taken` 来测——见 `unique_trash_name` 的注释。
    #[test]
    fn a_collision_appends_a_sequence_between_the_stamp_and_the_name() {
        let taken = |candidate: &str| candidate == "1700000000000-a.md";
        let name = unique_trash_name(1_700_000_000_000, "a.md", taken).unwrap();
        assert_eq!(name, "1700000000000-1-a.md");
        assert!(name.ends_with("-a.md"), "扩展名必须还在末尾：{name}");
    }

    /// 连续撞名：序号一路加，直到找到空位。
    #[test]
    fn consecutive_collisions_keep_counting_up() {
        let taken = |candidate: &str| {
            matches!(
                candidate,
                "1700000000000-a.md" | "1700000000000-1-a.md" | "1700000000000-2-a.md"
            )
        };
        assert_eq!(
            unique_trash_name(1_700_000_000_000, "a.md", taken).unwrap(),
            "1700000000000-3-a.md"
        );
    }

    /// 重试有上界：永远不会收敛的 `taken` 必须返回 `None`，不能死循环。
    #[test]
    fn an_always_taken_name_gives_up_instead_of_spinning() {
        assert!(
            unique_trash_name(1_700_000_000_000, "a.md", |_| true).is_none(),
            "无上界的 while 会让删除永久卡住，而它看起来像卡死不像出错"
        );
    }
}
