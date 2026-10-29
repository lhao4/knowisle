//! 转存为 UTF-8：ADR-010 那条红线的**唯一例外**。
//!
//! # 这条路径为什么必须存在
//!
//! [`crate::encoding`] 与 [`crate::write`] 立下的规矩是「保存写回原编码，
//! 装不下就拒绝」。那条规矩保护的是「用户没同意过的事我们不做」，代价是
//! 两种笔记被堵在死胡同里：
//!
//! | 病灶 | 表现 | 用户能做什么 |
//! |---|---|---|
//! | GBK 里加 emoji | 保存被拒（`ENCODING_NOT_WRITABLE`） | 去外部编辑器另存，或者删掉自己想写的字 |
//! | Big5 重复映射（12 组，如 `A2 A4` 与 `F9 F9` 都是「═」） | **任何**保存都被拒 | 连改一个错别字都做不到 |
//!
//! 转存是那条死胡同的出口，也是**唯一**允许改磁盘编码的路径。它必须同时
//! 满足三条，缺一条就不许合并：
//!
//! 1. **显式确认**：用户点两次（入口按钮 + 二次确认框），不存在「打开就转」
//!    「保存就转」「后台批量转」。
//! 2. **先备份**：`.knowisle/history/<rel>/<unix_ms>.md`，逐字节复制原文件。
//!    没有备份的转存不可撤销，而这一刀的全部价值就是可撤销。
//! 3. **原子写**：复用 [`crate::write`] 的 `tmp → fsync → rename`，不自己写 rename。
//!
//! # 幂等是契约，不只是优化
//!
//! 对已经是 UTF-8 的文件调用 = no-op：一个字节都不写、mtime 不变、
//! `history/` 下不多一份文件。这条保证「同一篇最多被转存一次」，于是
//! **不需要保留策略**——`history/` 里每篇最多一份。将来若真有人能证明
//! 同一篇会被转存两次（外部工具改回 GBK 再转），那时再回来加「留最近 20 份」。
//!
//! # 为什么不给 `write_note_checked` 加一个「目标编码」参数
//!
//! 加了那个参数就等于给「静默转码」留了一个可以传错的口子，而 ADR-010 的
//! 整个论证都建立在「保存不收编码参数」上。转存是**独立写路径**，它只写
//! 一种编码：UTF-8，不带 BOM。
//!
//! # 为什么前端不传编码
//!
//! 前端只说「转成 UTF-8」。探测与编码全在这里——多一个字符串参数就是多一个
//! 可以传错的地方，而传错的后果是用户的笔记变成另一种编码。
//!
//! # 错误只有三种（外加路径校验的透传）
//!
//! `Conflict` / `TooLarge` / `Io`，加上「认不出编码」时透传的 `InvalidEncoding`。
//! 编码到 UTF-8 不可能失败（UTF-8 能表示任何 `str`），所以没有
//! `EncodingNotWritable` 这一档。

use std::path::{Path, PathBuf};

use ki_core::{KiError, NoteContent};

use crate::encoding;
use crate::path::{resolve_for_write, METADATA_DIR};
use crate::scan::{read_note, stat, MAX_NOTE_BYTES};
use crate::write::atomic_replace;

/// 历史备份目录（`.knowisle/history/`）。
///
/// 与 MVP-SCOPE §3.4 约定的「后悔药」是同一个目录：将来 S8 的历史版本功能
/// 会往同一棵树下写，所以这里不留一个只服务转存的私有名字。
pub const HISTORY_DIR: &str = "history";

/// 把 `rel` 这篇笔记**显式地**转存为 UTF-8，返回转存后的正文与磁盘真值。
///
/// # 流程（顺序是契约）
///
/// ```text
/// 冲突检查 → 读一次字节 → 解码 → 已是 UTF-8 就返回（幂等）
///          → 备份 → atomic_replace(UTF-8) → 重读一次
/// ```
///
/// - **冲突检查在最前**：过期 mtime 时磁盘一个字节都不动，`history/` 下也
///   没有残留。备份写在检查之后是刻意的——「写了再清掉」多一条会失败的路径。
/// - **只读一次**：编码从这次读到的字节判定，不再调 `existing_encoding`
///   （它会把整个文件再读一遍）。转存就是「读一遍 + 写一遍」。
/// - **备份逐字节复制原文件**，不是「把解码后的正文按原编码编回去」——
///   后者在 Big5 重复映射这类文件上根本不是原字节，那就不是备份。
/// - **重读一次**：`size` 与 `mtime` 都变了，回执必须是磁盘真值。前端拿它
///   替换 `activeNote`，否则下一次自动保存会拿旧 mtime 撞 `CONFLICT`。
///
/// # 幂等
///
/// 磁盘上已经是 UTF-8（含带 BOM 的形态）时直接返回，不写盘、不动 mtime、
/// 不写备份。带 BOM 的 UTF-8 也算「本来就是 UTF-8」：它是文件头的一种写法，
/// 不是另一种字符集，剥掉它属于另一件事。
///
/// # 参数
///
/// `expected_mtime` 与 [`crate::write_note_checked`] 同一判据：`None` 表示
/// 无条件覆盖（CLI 用），`Some(t)` 时磁盘 mtime 必须仍等于 `t`。
pub fn transcode_to_utf8(
    root: &Path,
    rel: &str,
    expected_mtime: Option<i64>,
) -> Result<NoteContent, KiError> {
    let target = resolve_for_write(root, rel)?;

    // 一次 open 拿到 handle，正文与元数据都从它取（`fstat` 不碰路径）。
    let mut file = match std::fs::File::open(&target) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // 调用方读到过的文件现在没了：那不是「新建」，是冲突。
            // `write_note_checked` 用的是同一条判据（`disk_mtime = None`）。
            return Err(match expected_mtime {
                Some(expected) => KiError::Conflict {
                    path: rel.to_owned(),
                    expected_mtime: expected,
                    disk_mtime: None,
                },
                None => KiError::io(rel, &err),
            });
        }
        Err(err) => return Err(KiError::io(rel, &err)),
    };

    let meta = file.metadata().map_err(|e| KiError::io(rel, &e))?;
    if !meta.is_file() {
        return Err(KiError::NotAFile {
            path: rel.to_owned(),
        });
    }
    if meta.len() > MAX_NOTE_BYTES {
        return Err(KiError::TooLarge {
            path: rel.to_owned(),
            size: meta.len(),
        });
    }
    let len = meta.len();
    let (_, disk_mtime) = stat(Some(meta));

    // 冲突检查紧挨着写盘，把 TOCTOU 窗口压到最小。
    if let Some(expected) = expected_mtime {
        if disk_mtime != expected {
            return Err(KiError::Conflict {
                path: rel.to_owned(),
                expected_mtime: expected,
                disk_mtime: Some(disk_mtime),
            });
        }
    }

    let mut bytes = Vec::with_capacity(len as usize);
    {
        use std::io::Read;
        file.read_to_end(&mut bytes).map_err(|e| KiError::io(rel, &e))?;
    }

    // 克隆一次是为了留住**原字节**去写备份：`decode` 会吃掉 `Vec`（UTF-8
    // 快路径上直接复用那块缓冲区）。上限是 `MAX_NOTE_BYTES`，一次 10 MB
    // 的 memcpy 与随后那次写盘同量级，不值得为它多读一遍文件。
    let decoded =
        encoding::decode(bytes.clone()).map_err(|_| KiError::InvalidEncoding {
            path: rel.to_owned(),
        })?;

    // 幂等：磁盘上本来就是 UTF-8 时什么都不做。
    if decoded.encoding.is_utf8() {
        return Ok(NoteContent {
            path: rel.to_owned(),
            content: decoded.text,
            decoded_from: None,
            size: bytes.len() as u64,
            mtime: disk_mtime,
        });
    }

    // 编码后的字节数才是「写进去多大」，与读盘时的字节数不是一回事
    // （GBK 两个字节能变成 UTF-8 的三个）。判据与 `write_note_checked` 一致。
    let utf8 = decoded.text.as_bytes();
    if utf8.len() as u64 > MAX_NOTE_BYTES {
        return Err(KiError::TooLarge {
            path: rel.to_owned(),
            size: utf8.len() as u64,
        });
    }

    write_backup(root, rel, &bytes)?;

    // 转存的目标是「最通用的形态」：UTF-8，**不带 BOM**。
    atomic_replace(&target, utf8).map_err(|e| KiError::io(rel, &e))?;

    // 重读而不是自己拼：`size` / `mtime` 都要磁盘真值，顺带证明写下去的字
    // 节确实是一篇能被读出来的 UTF-8 笔记。
    read_note(root, rel)
}

/// 备份路径：`.knowisle/history/<rel>/<unix_ms>.md`。
///
/// # 为什么按 `rel` 分组而不是平铺
///
/// `notes/a.md` → `.knowisle/history/notes/a.md/1730000000000.md`。
/// `rel` 本身就是一条合法路径，天然按笔记分组：`ls` 一下就能看到某篇的
/// 全部历史，不需要解析文件名。代价是「笔记改名之后历史不跟着走」——
/// 那属于 S8 的问题，不是这一刀的。
///
/// # 为什么不会被扫成笔记
///
/// `.knowisle` 在 `scan::IGNORED_DIRS` 里（与 `path::METADATA_DIR` 是同一个
/// 常量），所以备份进不了树、也进不了索引。这一条有用例钉住，不靠记忆。
///
/// # 时间戳为什么是毫秒
///
/// 秒级在「同一秒转存两篇」时不冲突（目录不同），但同一篇被外部改回 GBK
/// 再转存时会撞名并覆盖掉上一份备份——那正是备份不该做的事。毫秒把这条
/// 概率压到可以忽略，且仍然按时间排序。
fn backup_path(root: &Path, rel: &str, stamp_ms: i64) -> PathBuf {
    let mut path = root.join(METADATA_DIR).join(HISTORY_DIR);
    // 逐段 push：`rel` 已经过 `resolve_for_write` 的语法校验（无 `..`、
    // 无绝对路径），这里再按 `/` 切开只是为了让 Windows 上的分隔符也正确。
    for segment in rel.split('/') {
        path.push(segment);
    }
    path.push(format!("{stamp_ms}.md"));
    path
}

/// 把原文件逐字节复制进 `history/`，失败即整个转存失败。
///
/// # 为什么失败要中止
///
/// 没有备份的转存不可撤销，而「可撤销」是这一刀唯一的价值。所以顺序是
/// **先备份、后改文件**：备份写不进去（磁盘满、目录只读）就停在原地，
/// 用户拿到的是一次明确的 IO 错误，而不是一篇被改过、又回不去的笔记。
///
/// # 为什么用 `atomic_replace`
///
/// 半截的备份比没有备份更坏：它看起来像一份可以恢复的历史，实际是坏字节。
/// `tmp → fsync → rename` 保证 `history/` 里出现的每一份都是完整的。
fn write_backup(root: &Path, rel: &str, bytes: &[u8]) -> Result<(), KiError> {
    let backup = backup_path(root, rel, now_ms());
    let parent = backup.parent().ok_or_else(|| {
        KiError::Internal(format!("备份路径没有父目录：{}", backup.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|e| KiError::io(rel, &e))?;
    atomic_replace(&backup, bytes).map_err(|e| KiError::io(rel, &e))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 备份路径按 `rel` 分组，且落在 `.knowisle/history/` 下。
    #[test]
    fn backup_path_groups_by_note() {
        let path = backup_path(Path::new("/vault"), "notes/a.md", 1730000000000);
        assert_eq!(
            path,
            Path::new("/vault").join(".knowisle/history/notes/a.md/1730000000000.md")
        );
    }
}
