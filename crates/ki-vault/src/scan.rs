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
/// 名单里的名字分两类，忽略它们的理由并不相同：
///
/// - **其他工具的元数据目录**（`.git` / `.obsidian` / `.logseq` / `.trash` /
///   `.stversions`）：里面装的是别人工具的配置、备份和回收站，不是用户的笔记。
///   `.obsidian` 与 `logseq` 尤其关键——"打开现有的 Obsidian / Logseq 库"是最主要的
///   迁移入口，不忽略的话 `.obsidian/plugins/*/README.md`、`logseq/bak/*.md`
///   会整片混进树里，用户第一眼看到的是一堆别人的文档和重复笔记。
/// - **构建产物与依赖**（`node_modules` / `target` / `dist` / ...）：误把前端仓库
///   选成 Vault 时，这些目录能有几十万文件，剪掉它们是性能问题而不只是观感问题。
///
/// `.knowisle` 单独说一句：这是我们自己的元数据目录（索引、缓存、历史），
/// 它永远不该出现在笔记树里——这是"派生数据不属于用户内容"的边界。
///
/// 名单是硬编码的，两个方向都会误伤：该忽略的漏了，或者用户真有个叫
/// `dist`、`logseq` 的笔记目录被静默吞掉。后者目前无法配置，
/// 见 `docs/BACKLOG.md` 的 B-05。
const IGNORED_DIRS: &[&str] = &[
    // 其他工具的元数据与备份
    ".git",
    ".obsidian",
    ".logseq",
    // 不带点的 `logseq` 也要忽略：Logseq 的图谱结构就是 `<graph>/logseq/bak/*.md`，
    // 里面是 pages 的全量历史副本。误伤面是一个真的叫 `logseq` 的笔记目录——
    // 概率低，且一旦有人抱怨就走 B-05 的配置化，不要靠加名字解决。
    "logseq",
    ".trash",
    ".stversions",
    ".knowisle",
    // 构建产物与依赖
    "node_modules",
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

/// 树深硬上限。**这不是业务参数，是一条栈安全不变量。**
///
/// 有四层代码按"树有多深"递归：
/// `assemble` 组树、`FileNode` 的默认 `Drop` 递归释放、`serde_json` 序列化、
/// 以及前端的递归渲染。任何一层爆栈都是进程级 abort——栈溢出在 Rust 里不可捕获，
/// `catch_unwind` 救不回来，Tauri 里表现为主进程凭空消失。
///
/// 所以深度必须由编译期常量锁死，**不能交给调用方**。
/// [`ScanOptions::max_depth`] 只是"请求值"，由 [`scan_vault`] 收敛到这里。
///
/// 取值依据：人类可维护的目录层级实际是 5–8 层，128 已是极宽松的余量；
/// 同时 128 帧组树在 256KB 栈上实测安全（见本文件 `tests` 模块）。
pub const MAX_SAFE_DEPTH: usize = 128;

/// 把调用方请求的深度收敛到安全上限。
///
/// 唯一的收敛点就是这里——不要在别处再判断一次，否则不变量就有了两个真相。
fn clamp_depth(requested: usize) -> usize {
    requested.min(MAX_SAFE_DEPTH)
}

/// 扫描参数。默认值对 99% 的 Vault 都够用。
///
/// 唯一不能随意调大的是 [`ScanOptions::max_depth`]：它是请求值，会被收敛。
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// 文件数上限。防止病态目录（如误选了整个用户目录）把 UI 卡死。
    pub max_files: usize,
    /// 请求的遍历深度上限。**会被收敛到 [`MAX_SAFE_DEPTH`]，调大无效。**
    ///
    /// 不让它直接决定递归深度，是因为组树、`Drop`、serde 序列化都按树深递归，
    /// 而这个字段是 `pub` 的——CLI 参数与插件都能写它。安全性不能依赖入参。
    pub max_depth: usize,
    /// 是否跟随符号链接。默认 `false`——这是防死循环的关键。
    pub follow_links: bool,
    /// 是否剪掉"没有任何笔记"的空目录。
    ///
    /// 默认 `false`：**磁盘上存在的目录就应该出现在树里**，树不该替用户判断
    /// 哪个目录值得看。M0 曾经默认 `true`（为了侧栏干净），但 M2 一加"新建文件夹"，
    /// 用户建完的空目录会立刻从侧栏消失，观感就是丢数据——所以这个默认值
    /// 在 M2 开工前必须翻过来。提前翻掉，不留地雷。
    ///
    /// 代价：纯附件目录（只有图片的 `assets/`）会显示成一个展开后什么都没有的文件夹。
    /// 将来若觉得吵，**不要改回 `true`**（那会重新引入"新建目录消失"），
    /// 正确做法是"保留 mtime 在 N 分钟内的空目录"——给新建目录一个宽限期，
    /// 历史空目录照剪。那时这个字段应该从 `bool` 变成一个小枚举。
    pub prune_empty_dirs: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_depth: 32,
            follow_links: false,
            prune_empty_dirs: false,
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

    // 唯一收敛点：无论调用方传 32 还是 usize::MAX，树深都不会超过 MAX_SAFE_DEPTH。
    // 这一行就是"栈安全不依赖入参"的全部实现，别把它删掉或挪到别处。
    let depth_limit = clamp_depth(opts.max_depth);

    let walker = WalkDir::new(&root_abs)
        .follow_links(opts.follow_links)
        .max_depth(depth_limit)
        // 确定性输出：同一份 Vault 每次扫描得到完全相同的顺序，CI 才能断言。
        .sort_by_file_name()
        .into_iter()
        .filter_entry(should_descend);

    for item in walker {
        let entry = match item {
            Ok(entry) => entry,
            // 权限不足、遍历中途文件消失、目录被并发删除：跳过这一个，继续走。
            //
            // 这里**没有**"软链成环"这种情况：`follow_links(false)` 下 WalkDir
            // 根本不进入软链，也就不会形成环。排查问题时别被这个念头带偏。
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
    assemble("", "", 0, flat, &by_parent, prune_empty_dirs)
}

/// 递归组装。**递归深度 == 树深，而树深由 [`MAX_SAFE_DEPTH`] 硬约束。**
///
/// `depth` 参数与末尾的 `debug_assert` 不是防御性代码，而是把不变量钉在消费点上：
/// 只要有人绕过 [`scan_vault`] 直接调 `build_tree`，debug 构建会当场炸，
/// 而不是等线上出现一次无从排查的 `drop_in_place` 栈溢出。
fn assemble(
    name: &str,
    path: &str,
    depth: usize,
    flat: &[RawEntry],
    by_parent: &HashMap<String, Vec<usize>>,
    prune_empty_dirs: bool,
) -> FileNode {
    debug_assert!(
        depth <= MAX_SAFE_DEPTH,
        "树深 {depth} 超过 MAX_SAFE_DEPTH={MAX_SAFE_DEPTH}：组树 / Drop / serde 都会按此递归"
    );

    let mut children = Vec::new();

    if let Some(indices) = by_parent.get(path) {
        for &index in indices {
            let entry = &flat[index];
            if entry.is_dir {
                let child = assemble(
                    &entry.name,
                    &entry.path,
                    depth + 1,
                    flat,
                    by_parent,
                    prune_empty_dirs,
                );
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
///
/// 同样是递归，但只作用于 [`build_tree`] 的产物，深度已被 [`MAX_SAFE_DEPTH`] 锁死。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 边界算术：这是"极端入参不会变成递归深度"的第一道证明。
    #[test]
    fn clamp_depth_bounds_every_input() {
        assert_eq!(clamp_depth(0), 0, "0 是合法请求：只要根目录");
        assert_eq!(clamp_depth(1), 1);
        assert_eq!(clamp_depth(32), 32, "默认值必须原样通过");
        assert_eq!(
            clamp_depth(MAX_SAFE_DEPTH),
            MAX_SAFE_DEPTH,
            "恰好等于上限时不该收敛"
        );
        assert_eq!(clamp_depth(MAX_SAFE_DEPTH + 1), MAX_SAFE_DEPTH);
        assert_eq!(clamp_depth(100_000), MAX_SAFE_DEPTH);
        assert_eq!(clamp_depth(usize::MAX), MAX_SAFE_DEPTH, "极端值必须被吃掉");
    }

    /// 造一条 `n` 层的纯目录链，用来压组树的递归。
    fn dir_chain(n: usize) -> Vec<RawEntry> {
        let mut flat = Vec::with_capacity(n);
        let mut path = String::new();
        for _ in 0..n {
            if !path.is_empty() {
                path.push('/');
            }
            path.push('d');
            flat.push(RawEntry {
                name: "d".to_owned(),
                path: path.clone(),
                is_dir: true,
                size: 0,
                mtime: 0,
            });
        }
        flat
    }

    fn tree_depth(node: &FileNode) -> usize {
        node.children
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|child| 1 + tree_depth(child))
            .max()
            .unwrap_or(0)
    }

    /// 最深合法树（正好 `MAX_SAFE_DEPTH` 层）必须能在 256KB 栈上组装完。
    ///
    /// 这条用例才是"不会栈溢出"的真正证明：栈只有默认值的 1/8。
    /// 将来谁把 `assemble` 的栈帧撑大、或把上限调高，它会立刻变红——
    /// 而且是以 abort 的形式变红，因为栈溢出不可捕获。
    #[test]
    fn deepest_legal_tree_assembles_within_a_small_stack() {
        std::thread::Builder::new()
            .name("deep-assemble".to_owned())
            .stack_size(256 * 1024)
            .spawn(|| {
                let flat = dir_chain(MAX_SAFE_DEPTH);
                // prune 必须关掉：纯目录链没有文件，剪空目录会把整条链剪没
                let tree = build_tree(&flat, false);
                assert_eq!(tree_depth(&tree), MAX_SAFE_DEPTH);
            })
            .expect("创建测试线程失败")
            .join()
            .expect("256KB 栈上组树不应崩溃");
    }
}
