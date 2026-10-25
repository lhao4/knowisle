//! 路径规范化、越界防护与读写边界。
//!
//! 这里有三个看起来微不足道、却决定安全边界的细节：
//!
//! 1. **分隔符**：`Path::to_string_lossy()` 在 Windows 上返回 `notes\a.md`。
//!    只要这个反斜杠漏到前端一次，`path` 作为 Map key 就会分裂成两份。
//! 2. **UNC 前缀**：`std::fs::canonicalize` 在 Windows 上返回 `\\?\E:\vault\...`。
//!    它和对话框返回的 `E:\vault` 做 `starts_with` 比较会永远为 false，
//!    于是所有"路径是否在 Vault 内"的检查静默失效。
//! 3. **重解析点**：`canonicalize` 会穿透软链与 junction 返回真实落点。
//!    穿透本身是防线的另一半（逃逸链接解析后落在库外，被 `starts_with` 拒绝），
//!    但它同时会**抹掉"这段路径是链接"这个事实**——所以要在穿透之前先逐级查一遍。
//!
//! 约定：所有路径转换只走 [`to_posix`]，所有规范化只走 `dunce::canonicalize`，
//! 所有路径语法校验只走 [`validate_rel`]。
//!
//! # 读写共用同一条可见边界
//!
//! 扫描器用 `follow_links(false)`，**不进入**任何重解析点——软链条目直接计入 `skipped`。
//! 解析器（本模块）与它对齐：路径上出现任何重解析点就拒绝。
//!
//! 这条对齐是 B-12 的结论。此前的行为是「扫描器不跟、解析器跟」，于是
//! `note_read("link/x.md")` 读得通、但 `link/` 在侧栏里根本看不见。
//! 当时不构成逃逸（目标本来就在库内），但一旦有了编辑器，它就升级成
//! **「改得到但看不见」**：用户可能在编辑一篇树里不存在的笔记，
//! 左侧高亮不到任何节点，而且写入会落到另一个路径上。
//!
//! 代价是每条路径多 k 次 `symlink_metadata`（k = 路径深度，通常 3–5）。
//! 相比「可见边界与可读边界不是同一条线」带来的困惑，这个代价是划算的。

use std::path::{Component, Path, PathBuf};

use ki_core::KiError;

/// 本应用的元数据目录（Vault 相对路径的第一级）。
///
/// 它同时出现在 `scan::IGNORED_DIRS` 里——两处引用同一个常量，是为了让
/// "扫描器看不见它" 与 "破坏性操作不许碰它" 不会各自漂移。
pub(crate) const METADATA_DIR: &str = ".knowisle";

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

/// 路径**语法**校验：只判断这段字符串有没有可能指向 Vault 内部，不碰文件系统。
///
/// 读写两条路径共用，确保规则只有一份——两份慢慢漂移的校验等于没有校验。
///
/// 拒绝的东西：
/// - `NUL` 字节（会让底层 syscall 直接报错，提前拒掉好给出明确错误）
/// - 绝对路径与带根路径（含 Windows 盘符、UNC、`\\?\` verbatim 前缀）
/// - 任何非 `Normal` / `CurDir` 的组件，其中 `ParentDir` 就是防目录穿越的第一道闸
fn validate_rel(rel: &str) -> Result<&Path, KiError> {
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

    for component in rel_path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            // `..` 落到这里被拒绝；`Prefix`（如 `C:foo` 这种盘符相对路径）同样拒绝。
            _ => {
                return Err(KiError::PathEscape {
                    path: rel.to_owned(),
                })
            }
        }
    }

    Ok(rel_path)
}

/// 拒绝 `.knowisle/` 下的路径。
///
/// # 为什么破坏性操作必须单独有这道闸
///
/// `resolve_in_vault` 只管"在不在库内"，`.knowisle/index.sqlite` 完全在库内、
/// 也完全存在，所以它能过那道闸。放行的话有两个后果：
///
/// - `note_delete(".knowisle/index.sqlite")` 能删掉索引（可重建，但用户会先
///   看到"搜索坏了"）；更糟的是删 `.knowisle/trash/x.md`，等于把已经删过一次的
///   东西再删一次，而它本来就是我们承诺"文件还在"的唯一证据。
/// - `note_rename(".knowisle/trash/x.md", "y")` 同理。
///
/// # 为什么大小写不敏感
///
/// 扫描器用 `eq_ignore_ascii_case` 匹配忽略名单（Windows 上 `.KnowIsle` 与
/// `.knowisle` 是同一个目录）。这里若用大小写敏感比较，就会出现
/// "扫描器看不见它、删除却删得掉"的裂缝——两边必须用同一把尺。
///
/// 分隔符同时认 `/` 与 `\`：`rel` 的契约是 POSIX，但 Windows 上 `Path::components`
/// 会把反斜杠也当分隔符，于是 `.knowisle\x.md` 能一路走到 rename。与其赌
/// 调用方永远不传反斜杠，不如在这里一次判掉。
pub(crate) fn reject_metadata_path(rel: &str) -> Result<(), KiError> {
    let first = rel.split(['/', '\\']).next().unwrap_or("");
    if first.eq_ignore_ascii_case(METADATA_DIR) {
        return Err(KiError::ReservedPath {
            path: rel.to_owned(),
        });
    }
    Ok(())
}

/// 从库根开始逐级检查 `rel`，任何一级是重解析点（软链 / junction / 挂载点）就拒绝。
///
/// # 为什么必须在 `canonicalize` **之前**跑
///
/// `canonicalize` 的职责就是把链接穿透成真实落点，穿透完就再也看不出
/// 「这段路径原本是个链接」了。等它跑完再检查，等于问一个已经把答案改掉的人
/// 原来的答案是什么。所以顺序是**先逐级查、再规范化**。
///
/// # 为什么不存在也算通过
///
/// 写路径允许目标文件尚不存在（新建笔记）。`symlink_metadata` 对不存在的路径
/// 返回 `ENOENT`，那不是"危险"，是"还没有"。真正的存在性要求由调用方
/// 对**父目录**的 `canonicalize` 负责。
///
/// # 为什么用 `symlink_metadata`
///
/// 它**不跟随**链接，读到的是链接自身而不是目标——正是这里要判断的东西。
/// Windows 上 junction 也会被 `is_symlink()` 判为 true（已实测，见
/// `tests/path.rs` 的 junction 用例）。
fn reject_reparse_points(root: &Path, rel: &Path, rel_str: &str) -> Result<(), KiError> {
    let mut current = root.to_path_buf();
    for component in rel.components() {
        let Component::Normal(segment) = component else {
            continue;
        };
        current.push(segment);
        if let Ok(meta) = std::fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(KiError::PathEscape {
                    path: rel_str.to_owned(),
                });
            }
        }
    }
    Ok(())
}

/// 把前端传来的 Vault 相对路径解析为绝对路径，并保证它没有逃出 Vault。
///
/// 这是所有"按路径**读**"操作的唯一入口。绕过它直接 `root.join(user_input)`
/// 等于把任意文件读取漏洞交给前端。
///
/// # 软链语义
///
/// 路径上任何一级是重解析点就直接拒绝，与扫描器 `follow_links(false)` 对齐。
/// 详见模块文档的「读写共用同一条可见边界」。
///
/// 注意 `starts_with` 是**组件比较**而非字符串前缀比较，
/// 所以 `vault-evil` 不会被误判为 `vault` 的子路径。
///
/// # 已知边界：TOCTOU
///
/// 检查（逐级 `symlink_metadata` + `canonicalize` + `starts_with`）与使用
/// （`fs::read`）之间存在时间窗。要在窗口内做手脚，攻击者需要对 Vault 目录有
/// 写权限——而那时他直接改笔记更省事。
/// 因此这里不做 fd 级校验（`O_NOFOLLOW` / `GetFinalPathNameByHandle` 二次确认），
/// 收益与复杂度不成比例。这是**有意识的取舍**，不是遗漏。
///
/// # 写入请用 [`resolve_for_write`]
///
/// 本函数要求目标**已存在**（`canonicalize` 的前提），因此无法用于新建笔记。
/// 写入路径的额外约束见 [`resolve_for_write`]。
pub fn resolve_in_vault(root: &Path, rel: &str) -> Result<PathBuf, KiError> {
    let rel_path = validate_rel(rel)?;

    let canonical_root = dunce::canonicalize(root)
        .map_err(|e| KiError::io(root.to_string_lossy().into_owned(), &e))?;

    // 第一道闸：路径上不许有任何重解析点。
    reject_reparse_points(&canonical_root, rel_path, rel)?;

    // 第二道闸：规范化后必须仍在 Vault 内。
    // 没有这一步，Vault 里一个指向 `C:\Windows` 的软链就能让任意读取成立。
    let canonical = dunce::canonicalize(root.join(rel_path)).map_err(|e| KiError::io(rel, &e))?;

    if !canonical.starts_with(&canonical_root) {
        return Err(KiError::PathEscape {
            path: rel.to_owned(),
        });
    }

    Ok(canonical)
}

/// 把 Vault 相对路径解析为**可写入**的绝对路径。
///
/// 与 [`resolve_in_vault`] 的两点不同，每一点都对应一个真实的失败场景：
///
/// 1. **目标文件可以不存在**（新建笔记），但父目录必须存在。
///    `resolve_in_vault` 依赖 `canonicalize`，天然做不到这一点。
/// 2. **目标是目录时提前报错**，而不是让 `fs::write` 抛一个裸 OS 错误。
///
/// # 重解析点：与读路径同一把闸
///
/// 路径上任何一级（含最终组件）是软链 / junction 都拒绝。这不是洁癖，
/// 是三条具体的伤害：
///
/// - **数据完整性**：`a.md -> b.md`（两者都在库内）。若放行，`note_save("a.md")`
///   会静默改写 `b.md`——用户以为在编辑 A，实际改坏了 B。两个文件都在库里，
///   用户不会怀疑边界，所以这种 bug 比逃逸更难被发现。
/// - **安全**：`a.md` 是指向库外的软链时，`fs::write` 会顺着它写到 Vault 之外，
///   而"父目录在库内"的检查照样通过。这是"只检查父目录"最典型的漏法。
/// - **一致性**：扫描器不进入软链，所以软链里的文件在侧栏里根本看不见。
///   若写入放行，用户就在改一篇"树里不存在"的笔记。
///
/// # 已知边界
///
/// - **硬链接无法在路径层识别**：硬链接和普通文件在路径上完全等价，要区分必须
///   比对 inode / file index，超出本函数的职责。这是路径层的固有局限。
/// - **TOCTOU 同上**：检查与写入之间仍有时间窗，取舍理由见 [`resolve_in_vault`]。
pub fn resolve_for_write(root: &Path, rel: &str) -> Result<PathBuf, KiError> {
    let rel_path = validate_rel(rel)?;

    // 没有文件名意味着目标是目录本身或空路径，写入无意义。
    let file_name = rel_path.file_name().ok_or_else(|| KiError::PathEscape {
        path: rel.to_owned(),
    })?;

    let canonical_root = dunce::canonicalize(root)
        .map_err(|e| KiError::io(root.to_string_lossy().into_owned(), &e))?;

    // 逐级拒绝重解析点，覆盖父目录与最终组件。
    reject_reparse_points(&canonical_root, rel_path, rel)?;

    // 父目录必须已存在：写入不会顺手替用户造目录，那属于上层的产品决策。
    let parent_rel = rel_path.parent().unwrap_or_else(|| Path::new(""));
    let canonical_parent =
        dunce::canonicalize(canonical_root.join(parent_rel)).map_err(|e| KiError::io(rel, &e))?;

    if !canonical_parent.starts_with(&canonical_root) {
        return Err(KiError::PathEscape {
            path: rel.to_owned(),
        });
    }

    let target = canonical_parent.join(file_name);

    // 目标是目录时提前给出可读错误。重解析点已经在上面的逐级检查里拒掉了，
    // 这里只剩"普通目录"这一种情况。
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_dir() {
            return Err(KiError::NotAFile {
                path: rel.to_owned(),
            });
        }
    }

    Ok(target)
}
