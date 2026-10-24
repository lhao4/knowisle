//! 集成测试共用的夹具。
//!
//! # 平台差异（必读）
//!
//! - **Unix**：`symlink(2)` 无需特权，测试全部真实执行。
//! - **Windows**：目录级链接用 **junction**（`mklink /J`），免管理员权限，稳定可用；
//!   文件级软链需要 `mklink`（要求开发者模式或 `SeCreateSymbolicLinkPrivilege`）。
//!   某些受限环境（CI 沙箱、被文件系统过滤驱动接管时）会**报成功却把链接拍平成
//!   普通文件**——所以 helper 一律在创建后用 `symlink_metadata` 复核，
//!   复核不过就打印原因并跳过，绝不相信命令的退出码。

#![allow(dead_code)]

use std::fs;
use std::path::Path;

/// 该路径是否真的是一个重解析点（软链 / junction）。
pub fn is_reparse_point(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// 创建目录级链接。返回 `false` 表示环境不支持，调用方应跳过。
pub fn try_link_dir(link: &Path, target: &Path) -> bool {
    let created = {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    };
    created && is_reparse_point(link)
}

/// 创建文件级链接。返回 `false` 表示环境不支持，调用方应跳过。
pub fn try_link_file(link: &Path, target: &Path) -> bool {
    let created = {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::process::Command::new("cmd")
                .args(["/C", "mklink"])
                .arg(link)
                .arg(target)
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    };
    // 关键：沙箱可能报成功却生成 0 字节普通文件，必须以文件系统状态为准
    created && is_reparse_point(link)
}

/// 环境不支持时打印一行原因并跳过本用例。
///
/// 注意 `cargo test` **不显示通过用例的 stdout**，所以"全绿"不等于"全跑过"。
/// 加 `--nocapture` 才看得到跳过了哪些。
macro_rules! skip_unless {
    ($created:expr, $what:expr) => {
        if !$created {
            eprintln!(
                "跳过：当前环境无法创建{}（需要符号链接权限，或被文件系统过滤驱动拦截）",
                $what
            );
            return;
        }
    };
}

pub(crate) use skip_unless;
