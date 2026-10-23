//! 路径规范化与越界防护。
//!
//! 这两个函数看起来微不足道，却是 Windows 上最容易被低估的两个坑：
//!
//! 1. **分隔符**：`Path::to_string_lossy()` 在 Windows 上返回 `notes\a.md`。
//!    只要这个反斜杠漏到前端一次，`path` 作为 Map key 就会分裂成两份。
//! 2. **UNC 前缀**：`std::fs::canonicalize` 在 Windows 上返回 `\\?\E:\vault\...`。
//!    它和对话框返回的 `E:\vault` 做 `starts_with` 比较会永远为 false，
//!    于是所有"路径是否在 Vault 内"的检查静默失效。
//!
//! 解决方案：所有路径转换只走 `to_posix`，所有规范化只走 `dunce::canonicalize`。

use std::path::{Component, Path, PathBuf};

use ki_core::KiError;

/// 把文件系统路径转成 POSIX 风格的相对路径。
///
/// 返回 `None` 表示路径中含有无法用 UTF-8 表示的片段。
/// 调用方必须**跳过**该条目，而不是 panic，也不是塞进 `U+FFFD` 替换字符——
/// 后者会让两个不同的文件在索引里变成同一个 key。
///
/// 中文与 emoji 都是合法 UTF-8，因此正常路径永远走 `Some` 分支；
/// `None` 只可能出现在 Linux 上由任意字节组成的文件名。
pub fn to_posix(rel: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(segment) => parts.push(segment.to_str()?),
            Component::CurDir => {}
            // ParentDir / RootDir / Prefix 在 strip_prefix 之后不该出现，
            // 出现了说明调用方传错了路径，宁可跳过也不要猜。
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// 把前端传来的 Vault 相对路径解析为绝对路径，并保证它没有逃出 Vault。
///
/// 这是所有"按路径读写"操作的唯一入口。绕过它直接 `root.join(user_input)`
/// 等于把任意文件读取漏洞交给前端。
pub fn resolve_in_vault(root: &Path, rel: &str) -> Result<PathBuf, KiError> {
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }

    // NUL 字节会让底层 syscall 直接报错，提前拒绝并给出明确错误。
    if rel.contains('\0') {
        return Err(KiError::PathEscape {
            path: rel.to_owned(),
        });
    }

    let rel_path = Path::new(rel);

    // 绝对路径（含 Windows 盘符与 UNC 前缀）一律拒绝。
    if rel_path.is_absolute() || rel_path.has_root() {
        return Err(KiError::PathEscape {
            path: rel.to_owned(),
        });
    }

    // 逐段校验：只放行普通片段与 `.`。
    // `..` 会落到 `_` 分支被拒绝，这是防目录穿越的第一道闸。
    for component in rel_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(KiError::PathEscape {
                    path: rel.to_owned(),
                })
            }
        }
    }

    // 第二道闸：解析符号链接后必须仍在 Vault 内。
    // 没有这一步，Vault 里一个指向 `C:\Windows` 的软链就能让任意读取成立。
    let canonical = dunce::canonicalize(root.join(rel_path)).map_err(|e| KiError::io(rel, &e))?;
    let canonical_root = dunce::canonicalize(root)
        .map_err(|e| KiError::io(root.to_string_lossy().into_owned(), &e))?;

    if !canonical.starts_with(&canonical_root) {
        return Err(KiError::PathEscape {
            path: rel.to_owned(),
        });
    }

    Ok(canonical)
}
