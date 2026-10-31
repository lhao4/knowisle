//! 库信息的**计算**：两次 `read_dir` + 若干次 `metadata`，不读正文、不开库。
//!
//! 类型在 `ki_core::info`（它跨 IPC 出去，形状属于契约），这里只负责「怎么算」。
//! 分开放的理由与 `trash` 一样：`ki-core` 不许碰 IO（公理 A2）。
//!
//! # 为什么是两次 `read_dir` 而不是一次全库遍历
//!
//! 面板要的是**这个应用自己占了多少**，不是「这个库有多大」。前者只看
//! `.knowisle/` 下面那两个目录（都平铺、都小），后者要遍历整个库——10k 篇
//! 的库那是一次秒级的 IO，为了一个诊断面板付这个代价不值。
//!
//! 笔记本身的体积**刻意不报**：那是用户的文件，不是应用占的空间。把两者
//! 混在一起，「库信息」会读起来像「磁盘占用」，而用户真正想问的是
//! 「我删掉的那些东西还占着多少」。

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use ki_core::{IndexState, KiError, VaultInfo, METADATA_DIR};
use ki_vault::TRASH_DIR;

/// 算一次快照。`index_state` 由调用方给（它是内存里的状态，不在这层）。
pub fn collect(root: &Path, index_state: IndexState) -> Result<VaultInfo, KiError> {
    let (trash_bytes, trash_entries) = trash_usage(root)?;
    Ok(VaultInfo {
        index_bytes: index_usage(root)?,
        trash_bytes,
        trash_entries,
        index_state,
    })
}

/// `.knowisle/` 下索引库占的字节数。
///
/// 认三种名字：`index.sqlite`、`index.sqlite-wal`、`index.sqlite-shm`。
/// **不能只认主库文件**：WAL 模式下正文可能几乎全在 `-wal` 里，只算主库会
/// 让一个正在建的库显示 0。
fn index_usage(root: &Path) -> Result<u64, KiError> {
    let dir = root.join(METADATA_DIR);
    let sidecar = format!("{}-", ki_store::DB_FILE);

    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        // 还没建过索引：`.knowisle/` 可能整个不存在。这不是错误，是 0。
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(KiError::io(dir.to_string_lossy(), &err)),
    };

    let mut total = 0;
    for entry in entries {
        // 单个条目读失败（并发删除、权限）：跳过。它下一秒可能就没了。
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name != ki_store::DB_FILE && !name.starts_with(&sidecar) {
            continue;
        }
        // `metadata` 而不是 `file_type`：要的是长度。它不跟随软链，
        // 与回收站那边同一个选择——软链指向的东西不该算进这个库。
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

/// 回收目录里的（字节数，文件数）。
///
/// 文件数与 `trash_list.total` **必须同口径**：都是「回收目录里一次 `read_dir`
/// 快照里的文件数」，都不含目录、不含枚举时读不出来的条目。两处显示同一个
/// 概念却差一，用户会以为其中一个坏了。
///
/// 不含 `trash-manifest/`：那是记录「它原来在哪」的元数据，不是用户以为
/// 「删了但还占着」的那部分。
fn trash_usage(root: &Path) -> Result<(u64, u64), KiError> {
    let dir = root.join(TRASH_DIR);

    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        // 从没删过东西：回收目录不存在。0 条、0 字节。
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok((0, 0)),
        Err(err) => return Err(KiError::io(TRASH_DIR, &err)),
    };

    let mut bytes = 0;
    let mut files = 0;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        bytes += meta.len();
        files += 1;
    }
    Ok((bytes, files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("knowisle-info-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    /// 索引库的三个文件都要算进来。
    ///
    /// 只算主库的写法在这里会得到 10 而不是 10+20+30——而真实的库在
    /// WAL 模式下，正文几乎全在 `-wal` 里。
    #[test]
    fn index_usage_counts_wal_and_shm() {
        let root = tmp("index");
        write(&root.join(METADATA_DIR).join("index.sqlite"), 10);
        write(&root.join(METADATA_DIR).join("index.sqlite-wal"), 20);
        write(&root.join(METADATA_DIR).join("index.sqlite-shm"), 30);
        // 干扰项：同目录下的别的文件不算。
        write(&root.join(METADATA_DIR).join("config.toml"), 999);
        assert_eq!(index_usage(&root).unwrap(), 60);
    }

    /// 没有索引库 = 0，不是错误。
    #[test]
    fn missing_metadata_dir_is_zero_not_an_error() {
        let root = tmp("no-index");
        assert_eq!(index_usage(&root).unwrap(), 0);
    }

    /// 回收站：只数文件、只数 `trash/`、不含清单目录。
    #[test]
    fn trash_usage_counts_files_only() {
        let root = tmp("trash");
        write(&root.join(TRASH_DIR).join("1700000000000-a.md"), 100);
        write(&root.join(TRASH_DIR).join("1700000000001-b.md"), 50);
        // 清单目录不算：它是元数据。
        write(&root.join(".knowisle/trash-manifest").join("x.json"), 7);
        // 目录不算。
        fs::create_dir_all(root.join(TRASH_DIR).join("sub")).unwrap();

        assert_eq!(trash_usage(&root).unwrap(), (150, 2));
    }

    /// 从没删过东西 = (0, 0)，不是错误。
    #[test]
    fn missing_trash_dir_is_zero() {
        let root = tmp("no-trash");
        assert_eq!(trash_usage(&root).unwrap(), (0, 0));
    }
}
