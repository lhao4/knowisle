//! 目录指纹：**树的形状**的一次廉价快照。
//!
//! # 它回答的问题
//!
//! 「自从上次扫描之后，磁盘上有没有多/少/改名过任何目录或笔记？」——
//! 只回答形状，不回答内容。内容变化归 `revalidate`（窗口聚焦时校验当前这一篇），
//! 两者刻意不互相调用：形状是整库的，内容是当前这一篇的。
//!
//! # 为什么不是目录 mtime（ADR-018 门禁②实测）
//!
//! | 动作 | 直接父目录 mtime | 库根 mtime |
//! |---|---|---|
//! | 新建 `sub/a.md` | 变 | **不变** |
//! | 普通 `writeFile` 覆盖 | 不变 | 不变 |
//! | **原子写 tmp→rename 覆盖** | **变** | 不变 |
//! | `sub/deep/` 里新建 / 删除 | 不变（`deep` 变） | 不变 |
//! | 删除 `sub/a.md` | 变 | 不变 |
//!
//! 三条结论：
//!
//! 1. **只比对库根 mtime 抓不到子目录里的任何增删**——而真实库的笔记几乎都在
//!    子目录里。原方案（B-09 ②「聚焦时比对根目录 mtime」）的形状不成立。
//! 2. 项目自己的保存是 `tmp → fsync → rename`，**每次保存都改所在目录的 mtime**。
//!    拿 mtime 当指纹 = 自己敲一个字就误判「外部变了」→ 每次聚焦全量重扫。
//! 3. 所以指纹取「所有目录的相对路径 + 该目录下的条目名集合」，**不取 mtime**。
//!    自己保存只改内容不改名字集合 → 天然不误触发；外部新建 / 删除 / **改名**
//!    都改名字集合 → 连改名都能抓到（比原方案更好）。
//!
//! # 成本
//!
//! 只 `read_dir`：不 stat 文件、不读正文。`DirEntry::file_type()` 在 Windows 上
//! 用的是 `FindFirstFileW` 已经带回来的属性，不额外发系统调用；Unix 上走
//! `d_type`，只有 `DT_UNKNOWN` 的文件系统才会补一次 `lstat`（极罕见）。
//! 10k 篇 / 100 目录的实测见 ADR-018 门禁①。
//!
//! # 哈希
//!
//! `DefaultHasher`：基线只活在**同一进程内**（前端 `useRef`），不落盘、不跨版本。
//! 换一个更强的哈希只是多一个依赖，换不到任何东西（对照 B-36 里「刻意不引入
//! 哈希」的理由）。**不许为此加依赖。**
//!
//! # 三条容易写错的
//!
//! - **名字必须排序**：Windows 的 `read_dir` 顺序不稳定，不排序会随机误判。
//!   子目录的**遍历顺序**同样要排——哈希是顺序敏感的。
//! - **大小写按原始字节比较**：NTFS 上 `a.md → A.md` 的改名本身就该被看见。
//! - **非 UTF-8 文件名用 `OsStr` 的字节**，不 `to_string_lossy`：那会把两个不同
//!   的名字压成同一个 `U+FFFD`。与扫描器一致——扫描器也**不把**这类条目放进树
//!   （`to_posix` 返回 `None`），所以这里同样跳过，指纹才等于树的形状。

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};

use ki_core::ignore::IgnoreRules;
use ki_core::KiError;

use crate::ignore::Matcher;
use crate::scan::{is_note, MAX_SAFE_DEPTH};

/// 哈希流里的段标记。有了它，`("ab","c")` 与 `("a","bc")` 不会撞。
const TAG_DIR: u8 = b'd';
const TAG_NAME: u8 = b'n';
/// 子目录读不出来。它变成可读之后这个标记消失 → 指纹变 → 触发一次重扫。
const TAG_UNREADABLE: u8 = b'!';

/// 整棵树的形状指纹，返回 16 位十六进制。
///
/// `rules` 必须与扫描器用的**是同一份**（`ignore::load(root)`）：否则
/// `.git` / `.knowisle` 里的变化会误触发重扫——自己写索引库触发自己。
///
/// # 失败
///
/// 库根读不出来 → `Err`（调用方**不许**在这种情况下刷新基线：库目录临时不可读
/// 时刷了基线，恢复之后就再也探不到了）。子目录读不出来只留一个标记，不失败。
pub fn dir_fingerprint(root: &Path, rules: &IgnoreRules) -> Result<String, KiError> {
    // 与 `scan_vault` 同一个规范化函数：Windows 上 `std::fs::canonicalize` 会
    // 返回 `\\?\E:\...`，两处算出来的相对路径前缀就不一样了。
    let root_abs =
        dunce::canonicalize(root).map_err(|err| KiError::io(root.to_string_lossy(), &err))?;
    let meta =
        fs::metadata(&root_abs).map_err(|err| KiError::io(root_abs.to_string_lossy(), &err))?;
    if !meta.is_dir() {
        return Err(KiError::NotADirectory {
            path: root_abs.to_string_lossy().into_owned(),
        });
    }

    let matcher = Matcher::new(rules);
    let mut hasher = DefaultHasher::new();
    walk(&root_abs, &[], 0, &matcher, &mut hasher)?;
    Ok(format!("{:016x}", hasher.finish()))
}

/// 深度优先走一遍目录，把「(相对路径, 排序后的条目名)」喂进哈希。
///
/// 递归深度由 [`MAX_SAFE_DEPTH`] 锁死：与扫描器同一个常量，理由也相同——
/// 栈溢出在 Rust 里不可捕获。扫描器的默认深度是 32，这里放到 128：
/// 比扫描更深的改动最多多触发**一次**重扫（扫完树没变、基线跟着前进），
/// 而比扫描更浅的改动一个都不会漏。
fn walk(
    dir: &Path,
    relative: &[u8],
    depth: usize,
    matcher: &Matcher,
    out: &mut DefaultHasher,
) -> Result<(), KiError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if depth == 0 => return Err(KiError::io(dir.to_string_lossy(), &err)),
        Err(_) => {
            feed(out, TAG_UNREADABLE, relative);
            return Ok(());
        }
    };

    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut children: Vec<(Vec<u8>, PathBuf)> = Vec::new();

    for entry in entries {
        // 单个目录项读失败（并发删除、权限）：跳过。下一次探测会看见真实形状。
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        // 与扫描器同一条判据：非 UTF-8 名字不进树（`to_posix` 返回 `None`）。
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            if matcher.ignores_dir(name_str) {
                continue;
            }
            let bytes = name.as_encoded_bytes().to_vec();
            names.push(bytes.clone());
            if depth < MAX_SAFE_DEPTH {
                children.push((bytes, entry.path()));
            }
            continue;
        }

        // 只认「扫描器会放进树的东西」：普通文件 + 笔记扩展名 + 没被忽略。
        // 软链 / 管道 / 设备文件在扫描器里是 `skipped`，不进树，这里也不进。
        // 代价是 `assets/photo.png` 增删不触发重扫——树本来也不会变。
        if file_type.is_file() && is_note(Path::new(name_str)) && !matcher.ignores_file(name_str) {
            names.push(name.as_encoded_bytes().to_vec());
        }
    }

    names.sort_unstable();
    feed(out, TAG_DIR, relative);
    for name in &names {
        feed(out, TAG_NAME, name);
    }

    // 遍历顺序也是哈希的一部分：不排的话 Windows 上同一棵树会算出不同的指纹。
    children.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (name, path) in children {
        let child = if relative.is_empty() {
            name
        } else {
            let mut joined = Vec::with_capacity(relative.len() + 1 + name.len());
            joined.extend_from_slice(relative);
            joined.push(b'/');
            joined.extend_from_slice(&name);
            joined
        };
        walk(&path, &child, depth + 1, matcher, out)?;
    }

    Ok(())
}

/// 把一段字节连同它的长度喂进哈希：长度前缀让段与段之间不会串味。
fn feed(hasher: &mut DefaultHasher, tag: u8, bytes: &[u8]) {
    hasher.write_u8(tag);
    hasher.write_u64(bytes.len() as u64);
    hasher.write(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn rules() -> IgnoreRules {
        IgnoreRules::builtin_only()
    }

    fn fp(dir: &TempDir) -> String {
        dir_fingerprint(dir.path(), &rules()).expect("算指纹失败")
    }

    fn write(dir: &TempDir, rel: &str, body: &str) {
        let abs = dir.path().join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, body).unwrap();
    }

    /// 同样的形状、不同的创建顺序 → 同一个指纹。
    ///
    /// 这条是「Windows 的 readdir 顺序不稳定」的守门人：名字与子目录顺序
    /// 任何一处没排，它都会随机红。
    #[test]
    fn same_shape_in_different_creation_order_hashes_the_same() {
        let a = TempDir::new().unwrap();
        for name in ["z.md", "a.md", "m.md"] {
            write(&a, name, "x");
        }
        write(&a, "sub/2.md", "x");
        write(&a, "sub/1.md", "x");

        let b = TempDir::new().unwrap();
        write(&b, "sub/1.md", "x");
        write(&b, "sub/2.md", "x");
        for name in ["m.md", "a.md", "z.md"] {
            write(&b, name, "x");
        }

        assert_eq!(fp(&a), fp(&b));
    }

    /// 判据 4 的地基：**改内容不改指纹**。
    ///
    /// 自己保存走 `tmp → fsync → rename`，会改目录 mtime 但不改名字集合。
    /// 这条红了就说明指纹里混进了 mtime 或内容。
    #[test]
    fn editing_content_does_not_change_the_fingerprint() {
        let dir = TempDir::new().unwrap();
        write(&dir, "notes/a.md", "第一版");
        let before = fp(&dir);

        write(&dir, "notes/a.md", "第二版，长得多的一段正文");
        assert_eq!(fp(&dir), before, "改内容不该改形状");
    }

    /// 判据 1：子目录里的新建必须被看见（根目录 mtime 抓不到它）。
    #[test]
    fn a_new_note_deep_in_a_subdirectory_changes_the_fingerprint() {
        let dir = TempDir::new().unwrap();
        write(&dir, "notes/a.md", "x");
        let before = fp(&dir);

        write(&dir, "notes/deep/deeper/b.md", "x");
        assert_ne!(fp(&dir), before);
    }

    /// 判据 3：改名 = 名字集合变了。内容一个字节都没动。
    #[test]
    fn renaming_a_note_changes_the_fingerprint() {
        let dir = TempDir::new().unwrap();
        write(&dir, "notes/a.md", "同一份正文");
        let before = fp(&dir);

        fs::rename(dir.path().join("notes/a.md"), dir.path().join("notes/b.md")).unwrap();
        assert_ne!(fp(&dir), before);
    }

    /// 删除一个空目录也要被看见：`prune_empty_dirs = false`，空目录在树里。
    #[test]
    fn an_empty_directory_is_part_of_the_shape() {
        let dir = TempDir::new().unwrap();
        let before = fp(&dir);
        fs::create_dir(dir.path().join("空目录")).unwrap();
        assert_ne!(fp(&dir), before, "空目录也是树的一部分");
        fs::remove_dir(dir.path().join("空目录")).unwrap();
        assert_eq!(fp(&dir), before, "删掉之后回到原样");
    }

    /// 判据 4 的另一半（变异②的守门人）：`.knowisle` 与 `.git` 里的变化
    /// **不许**触发重扫——否则自己建索引会触发自己。
    #[test]
    fn changes_inside_ignored_directories_do_not_change_the_fingerprint() {
        let dir = TempDir::new().unwrap();
        write(&dir, "a.md", "x");
        let before = fp(&dir);

        write(&dir, ".knowisle/index.sqlite", "not really sqlite");
        write(&dir, ".knowisle/trash/1700000000000-x.md", "x");
        write(&dir, ".git/objects/ab/cdef", "x");
        assert_eq!(fp(&dir), before, ".knowisle / .git 必须被忽略规则剪掉");
    }

    /// 用户配置的 `extra` 同样生效：指纹与扫描器共用一份 `IgnoreRules`。
    #[test]
    fn user_ignore_rules_apply_to_the_fingerprint() {
        let dir = TempDir::new().unwrap();
        write(&dir, "a.md", "x");
        let custom = IgnoreRules::with_user(vec!["drafts".to_owned()], Vec::new());
        let before = dir_fingerprint(dir.path(), &custom).unwrap();

        write(&dir, "drafts/x.md", "x");
        assert_eq!(
            dir_fingerprint(dir.path(), &custom).unwrap(),
            before,
            "extra 里的目录不许进指纹"
        );
        // 反面：同一份磁盘，用内置名单（不忽略 drafts）算出来的必然不同。
        assert_ne!(dir_fingerprint(dir.path(), &rules()).unwrap(), before);
    }

    /// 非笔记文件不进指纹：树的形状只由目录与 `.md` 决定。
    #[test]
    fn non_note_files_do_not_change_the_fingerprint() {
        let dir = TempDir::new().unwrap();
        write(&dir, "a.md", "x");
        // 目录先建好：新目录本身是形状的一部分，那不是这条要测的东西。
        fs::create_dir(dir.path().join("assets")).unwrap();
        let before = fp(&dir);

        write(&dir, "assets/photo.png", "x");
        assert_eq!(fp(&dir), before, "图片不进树，也就不该触发重扫");
        // 但同一张图片放进一个**新目录**里，那个目录本身要算数。
        write(&dir, "gallery/photo.png", "x");
        assert_ne!(fp(&dir), before, "新目录进树，必须触发");
    }

    /// 库根读不出来 → `Err`（调用方据此**不刷**基线）。
    #[test]
    fn a_missing_root_is_an_error() {
        let dir = TempDir::new().unwrap();
        let gone = dir.path().join("gone");
        assert!(dir_fingerprint(&gone, &rules()).is_err());
    }

    /// 指纹是稳定的十六进制，不是随机值（同一份磁盘连算两次必须相同）。
    #[test]
    fn the_fingerprint_is_deterministic() {
        let dir = TempDir::new().unwrap();
        write(&dir, "a.md", "x");
        let first = fp(&dir);
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(fp(&dir), first);
    }
}
