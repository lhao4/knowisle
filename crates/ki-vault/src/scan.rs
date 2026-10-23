//! Vault 扫描器。
//!
//! 设计要点（每一条都对应一个真实会踩的坑）：
//!
//! - **剪枝而不是过滤**：命中 `.git` 时直接不下降（`filter_entry`），
//!   而不是遍历完再丢弃。在一个带 `.git` 的 Vault 上这是 10 倍差距。
//! - **不跟随符号链接**：否则一个指向父目录的软链会让遍历无限递归。
//! - **错误只跳过不传播**：单个文件权限不足不该让整次扫描失败。
//! - **路径一律 POSIX**：见 `crate::path` 的说明。
//! - **非 UTF-8 文件名跳过**：绝不 `unwrap()`，也绝不塞替换字符。

use std::collections::HashMap;
use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

use walkdir::{DirEntry, WalkDir};

use ki_core::{FileNode, KiError, NoteContent, ScanResult};

use crate::path::{resolve_in_vault, to_posix};

/// 目录级忽略名单：命中即**不再下降**。
///
/// `.knowisle` 是我们自己的元数据目录（索引、缓存、历史），
/// 它永远不该出现在笔记树里——这是"派生数据不属于用户内容"的边界。
const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".knowisle",
    "target",
    "dist",
    ".next",
    ".turbo",
    ".cache",
    ".venv",
    "__pycache__",
];

/// 文件级忽略名单：纯噪音，连元数据都不必读。
const IGNORED_FILES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

/// 纳入笔记树的扩展名（大小写不敏感）。
const NOTE_EXTENSIONS: &[&str] = &["md", "markdown"];

/// 单篇笔记读取上限。超过就拒绝，避免用户误点一个 2GB 的假 `.md` 把内存打爆。
const MAX_NOTE_BYTES: u64 = 10 * 1024 * 1024;

/// 扫描参数。全部可调，但默认值对 99% 的 Vault 都够用。
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// 文件数上限。防止病态目录（如误选了整个用户目录）把 UI 卡死。
    pub max_files: usize,
    /// 遍历深度上限，同时构成组树递归深度的上界。
    pub max_depth: usize,
    /// 是否跟随符号链接。默认 `false`——这是防死循环的关键。
    pub follow_links: bool,
    /// 是否剪掉"没有任何笔记"的空目录。
    ///
    /// M0 默认 `true`（侧栏干净）。M2 引入"新建文件夹"后必须改成 `false`，
    /// 否则用户新建的空目录会立刻消失。
    pub prune_empty_dirs: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_depth: 32,
            follow_links: false,
            prune_empty_dirs: true,
        }
    }
}

/// 扫描时用的扁平中间表示。先收平再组树，避免边遍历边构造嵌套结构。
struct RawEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    mtime: i64,
}

/// 扫描一个 Vault，返回 POSIX 相对路径的树。
pub fn scan_vault(root: &Path, opts: &ScanOptions) -> Result<ScanResult, KiError> {
    let started = Instant::now();

    // `dunce::canonicalize` 而不是 `std::fs::canonicalize`：后者在 Windows 上
    // 会返回 `\\?\E:\...`，前端拿到会一脸懵，且后续 `starts_with` 比较会失效。
    let root_abs =
        dunce::canonicalize(root).map_err(|e| KiError::io(root.to_string_lossy(), &e))?;

    let root_meta =
        std::fs::metadata(&root_abs).map_err(|e| KiError::io(root_abs.to_string_lossy(), &e))?;
    if !root_meta.is_dir() {
        return Err(KiError::NotADirectory {
            path: root_abs.to_string_lossy().into_owned(),
        });
    }

    let mut flat: Vec<RawEntry> = Vec::new();
    let mut skipped: usize = 0;
    let mut truncated = false;

    let walker = WalkDir::new(&root_abs)
        .follow_links(opts.follow_links)
        .max_depth(opts.max_depth)
        // 确定性输出：同一份 Vault 每次扫描得到完全相同的顺序，CI 才能断言。
        .sort_by_file_name()
        .into_iter()
        .filter_entry(should_descend);

    for item in walker {
        let entry = match item {
            Ok(entry) => entry,
            // 权限不足 / 遍历中途文件消失 / 软链成环：跳过这一个，继续走。
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        if entry.depth() == 0 {
            continue; // 根目录自身不入树
        }

        let relative = match entry.path().strip_prefix(&root_abs) {
            Ok(relative) => relative,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let path = match to_posix(relative) {
            Some(path) => path,
            None => {
                // 非 UTF-8 文件名：跳过，绝不 panic。
                skipped += 1;
                continue;
            }
        };

        let file_type = entry.file_type();
        let is_dir = file_type.is_dir();

        if !is_dir && !file_type.is_file() {
            // 管道 / socket / 设备文件，不是笔记。
            skipped += 1;
            continue;
        }

        // 目录必须全收：否则中间层级断裂，树就接不起来。
        if !is_dir && !is_note(entry.path()) {
            continue;
        }

        let name = match entry.file_name().to_str() {
            Some(name) => name.to_owned(),
            None => {
                skipped += 1;
                continue;
            }
        };

        if flat.len() >= opts.max_files {
            truncated = true;
            break;
        }

        let (size, mtime) = stat(entry.metadata().ok());
        flat.push(RawEntry {
            name,
            path,
            is_dir,
            size,
            mtime,
        });
    }

    let tree = build_tree(&flat, opts.prune_empty_dirs);
    let (total_files, total_dirs) = count_tree(&tree);

    Ok(ScanResult {
        root: root_abs.to_string_lossy().into_owned(),
        tree,
        total_files,
        total_dirs,
        truncated,
        skipped,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// 读取单篇笔记。
///
/// 路径必须经由 `resolve_in_vault`，因此天然拒绝目录穿越。
pub fn read_note(root: &Path, rel: &str) -> Result<NoteContent, KiError> {
    let absolute = resolve_in_vault(root, rel)?;
    let meta = std::fs::metadata(&absolute).map_err(|e| KiError::io(rel, &e))?;

    if !meta.is_file() {
        return Err(KiError::Io {
            path: rel.to_owned(),
            message: "不是普通文件".to_owned(),
        });
    }
    if meta.len() > MAX_NOTE_BYTES {
        return Err(KiError::TooLarge {
            path: rel.to_owned(),
            size: meta.len(),
        });
    }

    let bytes = std::fs::read(&absolute).map_err(|e| KiError::io(rel, &e))?;
    let content = String::from_utf8(bytes).map_err(|_| KiError::InvalidEncoding {
        path: rel.to_owned(),
    })?;

    // Windows 记事本保存 UTF-8 时会写 BOM。不去掉的话，第一个 `# 标题`
    // 前面多了个不可见字符，大纲解析与"首个标题"逻辑会同时失效。
    let content = match content.strip_prefix('\u{feff}') {
        Some(stripped) => stripped.to_owned(),
        None => content,
    };

    let (size, mtime) = stat(Some(meta));
    Ok(NoteContent {
        path: rel.to_owned(),
        content,
        size,
        mtime,
    })
}

/// 是否应该继续下降。
///
/// `depth() == 0` 永远放行——用户完全可能选了一个名叫 `dist` 或 `target`
/// 的目录作为 Vault，此时把根剪掉会导致"选了文件夹却什么都没有"。
fn should_descend(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if entry.file_type().is_dir() {
        !IGNORED_DIRS
            .iter()
            .any(|ignored| ignored.eq_ignore_ascii_case(name.as_ref()))
    } else {
        !IGNORED_FILES
            .iter()
            .any(|ignored| ignored.eq_ignore_ascii_case(name.as_ref()))
    }
}

fn is_note(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            NOTE_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
}

/// 取 `(size, mtime_ms)`。元数据读失败时退化为 `(0, 0)`，不影响扫描继续。
fn stat(meta: Option<std::fs::Metadata>) -> (u64, i64) {
    let Some(meta) = meta else {
        return (0, 0);
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    (meta.len(), mtime)
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

/// 目录在前，然后按名称不区分大小写排序，最后用原名兜底保证稳定。
///
/// 注意：中文按 Unicode 码位排序，不是拼音序。M0 接受这一点；
/// 真正的自然排序（拼音 / 数字感知）等有用户抱怨再说。
fn compare_entries(a: &RawEntry, b: &RawEntry) -> std::cmp::Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.name.cmp(&b.name))
}

fn build_tree(flat: &[RawEntry], prune_empty_dirs: bool) -> FileNode {
    let mut by_parent: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, entry) in flat.iter().enumerate() {
        by_parent
            .entry(parent_of(&entry.path).to_owned())
            .or_default()
            .push(index);
    }
    for indices in by_parent.values_mut() {
        indices.sort_by(|&a, &b| compare_entries(&flat[a], &flat[b]));
    }
    assemble("", "", flat, &by_parent, prune_empty_dirs)
}

/// 递归组装。深度受 `max_depth` 约束（默认 32），不会栈溢出。
fn assemble(
    name: &str,
    path: &str,
    flat: &[RawEntry],
    by_parent: &HashMap<String, Vec<usize>>,
    prune_empty_dirs: bool,
) -> FileNode {
    let mut children = Vec::new();

    if let Some(indices) = by_parent.get(path) {
        for &index in indices {
            let entry = &flat[index];
            if entry.is_dir {
                let child = assemble(&entry.name, &entry.path, flat, by_parent, prune_empty_dirs);
                if !(prune_empty_dirs && child.is_empty_dir()) {
                    children.push(child);
                }
            } else {
                children.push(FileNode::file(
                    &entry.name,
                    &entry.path,
                    entry.size,
                    entry.mtime,
                ));
            }
        }
    }

    FileNode::dir(name, path, children)
}

/// 统计树里的文件数与目录数（不含根节点自身）。
fn count_tree(node: &FileNode) -> (usize, usize) {
    let mut files = 0;
    let mut dirs = 0;
    if let Some(children) = &node.children {
        for child in children {
            if child.kind.is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
            let (child_files, child_dirs) = count_tree(child);
            files += child_files;
            dirs += child_dirs;
        }
    }
    (files, dirs)
}
