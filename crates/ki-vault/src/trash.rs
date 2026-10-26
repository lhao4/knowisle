//! 应用内回收站：清单、列表、恢复、清空。
//!
//! 删除（[`crate::fileops::delete_note`]）只做一件事：把文件 `rename` 进
//! `.knowisle/trash/`。它**不知道**用户是从哪个目录删的——文件名里只有原名。
//! 本模块补上缺的那一半：一条「原名 → 原路径」的清单，以及三个动作。
//!
//! # 真相源的分工（这条决定了所有崩溃窗口的方向）
//!
//! **回收目录里有没有那个文件 = 它还在不在。**
//! **清单里有没有那条记录 = 知不知道它原来在哪。**
//!
//! 所以删除的顺序是**先搬文件、后写清单**，恢复的顺序是**先搬文件、后删清单**。
//! 两个窗口里丢的都只是"位置信息"，文件本身一个字节都不会丢——这正是它
//! 不违反公理 A1 的地方：正文始终在磁盘上，清单只描述位置（见 ADR-008）。
//!
//! 反过来的顺序（先写清单）会在崩溃时留下"清单说有、文件没有"的悬空记录，
//! 而那条记录会让列表显示一个**点开就报错**的条目。宁可少一条元数据，
//! 也不要多一条假条目。
//!
//! # 清单为什么是"一条一个文件"
//!
//! A5 要求 `tmp → fsync → rename`，而 rename 只能**整体替换**一个文件；
//! 追加写（`O_APPEND`）则做不到原子替换。两条要求放在一个文件上是互斥的。
//!
//! 于是把「追加」下沉到目录层：`.knowisle/trash-manifest/` 里一条记录一个
//! `<entry>.json`，新增一条 = 新增一个文件（走同一条 [`crate::write::atomic_replace`]），
//! 从不重写已有记录。三个好处都不是附赠的：
//!
//! 1. **单条损坏只损失一条**。单文件 JSON 里一个坏字节会让整份清单读不出来，
//!    用户的所有原路径一起消失。
//! 2. **O(1) 删除**。不需要"读全量 → 改 → 写全量"。
//! 3. **读侧天然自愈**：孤儿记录（文件已经不在回收目录里）在列表时被忽略，
//!    不需要任何清理任务。
//!
//! 代价是 1000 条删除会有 1000 个小文件。回收站不是数据库：一次 `read_dir`
//! 加 1000 次小文件读在本机是毫秒级，而它每天最多发生几次。

use std::io::ErrorKind;
use std::path::{Component, Path};
use std::time::UNIX_EPOCH;

use ki_core::{KiError, RestoreResult, TrashEntry};
use serde::{Deserialize, Serialize};

use crate::fileops::move_no_clobber;
use crate::path::{reject_metadata_path, resolve_for_write, validate_rel};
use crate::write::atomic_replace;

/// 回收目录（Vault 相对路径，POSIX 分隔符）。
///
/// 它落在 `.knowisle/` 下面，而 `.knowisle` 在 `scan::IGNORED_DIRS` 里——
/// 于是删掉的东西**天然**从树、快速打开、全文索引里消失，不需要任何额外过滤。
/// 这条"放在哪里"的决定比"删完之后还要记得过滤"可靠得多：过滤会漏，位置不会。
pub const TRASH_DIR: &str = ".knowisle/trash";

/// 清单目录（Vault 相对路径）。
///
/// # 为什么是 `trash` 的**兄弟**，不是 `trash/manifest`
///
/// 回收目录的布局是一条红线：`trash/` 里只躺被删掉的文件，平铺，谁都能一眼
/// 看懂。往里塞一个 `manifest/` 子目录会同时带来三个麻烦：`ls` 的结果里混进
/// 一个不是笔记的东西、所有"回收目录里有几个文件"的判断都要多一个例外、
/// 而用户拿资源管理器打开时会以为那个目录也是他删的。
///
/// 放在同一级（仍然在 `.knowisle/` 下）就都不存在：扫描器看不见 `.knowisle`，
/// 回收目录保持原样，清单也有自己的容身处。
pub const MANIFEST_DIR: &str = ".knowisle/trash-manifest";

/// 恢复撞名时，最多往后找多少个 `名字 2.md` 这样的候选。
///
/// 与 `fileops::TRASH_SEQ_LIMIT` 同一条理由：这是**收敛保证**，不是业务参数。
/// 循环必须有上界，否则一个被塞满同名文件的目录会让恢复永久卡住。
const TARGET_SEQ_LIMIT: u32 = 1000;

/// 清单里的一条记录。字段就是"回哪儿"和"什么时候删的"。
///
/// 不存 `entry`：文件名已经说明了它是哪一条。存一份副本只会多出一个
/// "两处不一致时信谁"的问题——而 Windows 大小写不敏感会让这种不一致
/// 在毫无征兆的情况下出现。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    /// 删除前的 Vault 相对路径（POSIX）。
    origin: String,
    /// 删除时间（Unix 毫秒）。
    deleted_at: i64,
}

/* ───────────────────── 清单读写 ───────────────────── */

fn manifest_dir(root: &Path) -> std::path::PathBuf {
    root.join(MANIFEST_DIR)
}

fn manifest_path(root: &Path, entry: &str) -> std::path::PathBuf {
    manifest_dir(root).join(format!("{entry}.json"))
}

/// 写入一条「`entry` 来自 `origin`」的记录。
///
/// 调用方必须在**文件已经进回收目录之后**调用。失败不是致命错误：
/// 文件还在，只是列表会退化成"原路径未知"——由调用方决定怎么报告。
pub(crate) fn record(
    root: &Path,
    entry: &str,
    origin: &str,
    deleted_at: i64,
) -> Result<(), KiError> {
    let dir = manifest_dir(root);
    std::fs::create_dir_all(&dir).map_err(|err| KiError::io(MANIFEST_DIR, &err))?;

    let body = serde_json::to_vec(&Record {
        origin: origin.to_owned(),
        deleted_at,
    })
    .map_err(|err| KiError::Internal(format!("回收站清单序列化失败：{err}")))?;

    let rel = format!("{MANIFEST_DIR}/{entry}.json");
    atomic_replace(&manifest_path(root, entry), &body).map_err(|err| KiError::io(rel, &err))
}

/// 删掉 `entry` 的记录。**失败无所谓**：读侧以回收目录为准，孤儿记录会被忽略。
pub(crate) fn forget(root: &Path, entry: &str) {
    let _ = std::fs::remove_file(manifest_path(root, entry));
}

/// 读一条记录。文件不存在、读不出来、JSON 坏了都返回 `None`。
///
/// 这是**降级路径的入口**：任何一种失败都只意味着"不知道它原来在哪"，
/// 而不是"这条回收站记录不可用"。用户至少还能把它恢复到库根目录。
fn read_record(root: &Path, entry: &str) -> Option<Record> {
    let bytes = std::fs::read(manifest_path(root, entry)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 记录里的原路径是否**可能**指向库内。
///
/// 复用 [`validate_rel`] 与 [`reject_metadata_path`]，不另写一份判断：
/// 清单是磁盘上的普通文件，用户（或某个坏掉的同步工具）可以改它。
/// 一条被改成 `../../.ssh/id_rsa` 的记录不该被显示成"原路径"，
/// 更不该被拿去恢复——真正的闸在 [`resolve_for_write`]，这里只是**显示前**的过滤。
fn plausible_origin(origin: &str) -> Option<String> {
    if validate_rel(origin).is_err() || reject_metadata_path(origin).is_err() {
        return None;
    }
    Some(origin.to_owned())
}

/* ───────────────────── 列表 ───────────────────── */

/// 列出回收站，按删除时间倒序（最新在前）。
///
/// # 顺序与判据
///
/// - **一条回收站记录 = 回收目录里的一个普通文件**。清单只提供附加信息，
///   不参与"有没有这一条"的判定。
/// - `deleted_at` 三级回退：清单 → 文件名前缀 → 文件 mtime。三级都失败才用 0。
///   所以它恒有值，前端不必为 `null` 写分支。
/// - `origin` 为 `None` 是**正常状态**，不是错误：清单缺失、损坏、或记录被
///   改成了越界路径都会落到这里。UI 必须把它显示成「原路径未知」。
///
/// # 单个条目失败不连坐
///
/// 读不出元数据的条目直接跳过（与扫描器同一条策略）：一个被占用的文件
/// 不该让整个回收站打不开。
pub fn list(root: &Path) -> Result<Vec<TrashEntry>, KiError> {
    let dir = root.join(TRASH_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // 没有回收目录 = 回收站是空的。这不是错误：从没删过东西的库就是这样。
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(KiError::io(TRASH_DIR, &err)),
    };

    let mut out = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else { continue };
        // 目录与软链不是"被删掉的笔记"。回收目录是平的，出现目录只可能是
        // 用户自己放进去的，把它列出来再让人"恢复"会搬出一个目录树。
        if !meta.is_file() {
            continue;
        }

        let record = read_record(root, &name);
        let deleted_at = record
            .as_ref()
            .map(|record| record.deleted_at)
            .filter(|stamp| *stamp > 0)
            .or_else(|| stamp_of(&name))
            .unwrap_or_else(|| mtime_ms(&meta));
        let origin = record.and_then(|record| plausible_origin(&record.origin));

        // 展示名在 Rust 侧算一次：有原路径取末段，没有就剥时间戳。
        // 前端只管渲染——两边各写一份剥法，迟早会在某个边界上不一致，
        // 而那种不一致不会报错，只会让列表里的名字和恢复出来的不一样。
        let display = origin
            .as_deref()
            .and_then(|origin| origin.rsplit('/').next())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| strip_stamp(&name));

        out.push(TrashEntry {
            entry: name,
            name: display,
            origin,
            deleted_at,
            size: meta.len(),
        });
    }

    // 倒序 + 条目名倒序：同一毫秒的两条也要有确定顺序，否则列表会在两次
    // 刷新之间无理由地换位置。条目名里的时间戳前缀让"同毫秒"天然有序
    // （展示名不行：两个目录下的同名文件展示名完全一样）。
    out.sort_by(|a, b| {
        b.deleted_at
            .cmp(&a.deleted_at)
            .then_with(|| b.entry.cmp(&a.entry))
    });
    Ok(out)
}

/* ───────────────────── 恢复 ───────────────────── */

/// 把回收目录里的 `entry` 搬回原目录，返回它最终落在哪里。
///
/// # 落点决策
///
/// | 情况 | 落到 | `degraded` |
/// |---|---|---|
/// | 清单有记录且原目录还在 | 原路径 | false |
/// | 清单缺失 / 损坏 / 越界 | 库根目录 + 从文件名剥出的原名 | true |
/// | 原目录已不存在 | 库根目录 + 原名 | true |
///
/// **不代建原目录**：删除只搬文件，目录本来就还在；目录没了说明是外部删的，
/// 这时悄悄重建一串目录比"落到根目录并明确告知"更容易让用户困惑。
/// 记在 BACKLOG。
///
/// # 撞名
///
/// 目标已存在时**绝不覆盖**，改用 `c.md → c 2.md`（与 ARCHITECTURE §3.2
/// 的重名规则一致）。改名本身不是错误，`RestoreResult::renamed` 会说明。
///
/// # 为什么不检查"目标是不是当前正在编辑的那篇"
///
/// 恢复不写内容、只搬目录项。编辑器持有的路径若正好是恢复目标，那它
/// 打开的一定是另一份文件（同名已存在 → 我们会改名），不会互相覆盖。
pub fn restore(root: &Path, entry: &str) -> Result<RestoreResult, KiError> {
    check_entry(entry)?;

    let canonical_root = dunce::canonicalize(root)
        .map_err(|err| KiError::io(root.to_string_lossy().into_owned(), &err))?;
    let source = canonical_root.join(TRASH_DIR).join(entry);
    let rel_source = format!("{TRASH_DIR}/{entry}");

    let meta =
        std::fs::symlink_metadata(&source).map_err(|err| KiError::io(rel_source.clone(), &err))?;
    if !meta.is_file() {
        return Err(KiError::NotAFile { path: rel_source });
    }

    // 1. 想回到哪。清单可信就用它，否则退到库根 + 剥掉时间戳前缀的名字。
    let known = read_record(&canonical_root, entry).and_then(|record| {
        let origin = plausible_origin(&record.origin)?;
        // 原目录可能已经被外部删掉。`resolve_for_write` 会替我们回答
        // "这条路还能不能落"——不能就降级，而不是报错让用户无从下手。
        resolve_for_write(&canonical_root, &origin).ok()?;
        Some(origin)
    });
    let degraded = known.is_none();
    let requested = known.unwrap_or_else(|| strip_stamp(entry));

    // 2. 撞名就换名字，绝不覆盖。判据是磁盘上有没有这个名字（不跟随软链，
    //    与 `fileops::unique_trash_name` 用同一把尺）。
    let mut path = requested.clone();
    if occupied(&canonical_root, &path) {
        path = unique_target(&path, |candidate| occupied(&canonical_root, candidate)).ok_or_else(
            || {
                KiError::Internal(format!(
                    "目标目录里已有 {TARGET_SEQ_LIMIT} 个同名文件，已放弃恢复：{requested}"
                ))
            },
        )?;
    }

    // 3. 搬。`move_no_clobber` 会再挡一次"检查之后目标才出现"的竞态。
    let target = resolve_for_write(&canonical_root, &path)?;
    move_no_clobber(&source, &target, &path)?;

    // 4. 文件已经回去了，才删清单。反过来的话，崩溃会留下"清单说有、
    //    文件没有"的悬空记录——那比丢一条元数据更糟。
    forget(&canonical_root, entry);

    Ok(RestoreResult {
        path: path.clone(),
        requested: requested.clone(),
        renamed: path != requested,
        degraded,
    })
}

/* ───────────────────── 清空 ───────────────────── */

/// 清空回收站，返回删掉了多少个文件。
///
/// # 只删文件
///
/// 回收目录是平的，我们只往里放文件。用户若自己往里放了一个目录，
/// **不递归删**——那不是我们造的东西，也不在"清空回收站"的承诺里。
/// 跳过比递归安全：递归删错一次的代价是不可逆的。
///
/// # 清单目录要单独清
///
/// 它不在回收目录里（见 [`MANIFEST_DIR`]）。漏了这一步，下一次删除会读到
/// 一堆孤儿记录——虽然读侧会忽略它们，但那是靠一条容错规则兜着，
/// 不如直接把状态收干净。
///
/// # 部分失败
///
/// 一个文件被别的程序占住时，其余条目照删，最后返回第一个错误。
/// 这比"遇到第一个错误就停"好：用户点清空的意图是"能删的都删掉"。
pub fn purge(root: &Path) -> Result<u64, KiError> {
    let dir = root.join(TRASH_DIR);
    let mut removed = 0u64;
    let mut first_error: Option<KiError> = None;

    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let Ok(entry) = entry else { continue };
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();

                // 目录不是我们造的，跳过。
                if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    continue;
                }

                match std::fs::remove_file(&path) {
                    Ok(()) => removed += 1,
                    Err(err) => {
                        let rel = format!("{TRASH_DIR}/{name}");
                        first_error.get_or_insert_with(|| KiError::io(rel, &err));
                    }
                }
            }
        }
        // 没有回收目录 = 没有东西可清。清单目录可能还在（比如只删过一次又
        // 手工挪走了文件），所以这里不 early return，继续去清清单。
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(KiError::io(TRASH_DIR, &err)),
    }

    if manifest_dir(root).is_dir() {
        if let Err(err) = std::fs::remove_dir_all(manifest_dir(root)) {
            first_error.get_or_insert_with(|| KiError::io(MANIFEST_DIR, &err));
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(removed),
    }
}

/* ───────────────────── 纯函数（可确定性测试） ───────────────────── */

/// 恢复入口的路径闸：`entry` 必须是**回收目录里的单个文件名**。
///
/// 前端传来的是列表里的 `name`，但 IPC 是公开边界：`trash_restore(root, "../a.md")`
/// 若被放行，就能把库外的文件搬进库（或反过来）。所以这里用 `Path::components`
/// 判"恰好一个普通组件"——它同时挡掉 `..`、`/`、`\`、盘符与绝对路径，
/// 而且 Windows 上反斜杠也被当成分隔符，不需要再写一份平台分支。
fn check_entry(entry: &str) -> Result<(), KiError> {
    let mut parts = Path::new(entry).components();
    let single = matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none();
    if entry.is_empty() || !single {
        return Err(KiError::PathEscape {
            path: entry.to_owned(),
        });
    }
    Ok(())
}

/// `<unix_ms>-<原名>` → `<原名>`。
///
/// 清单丢失时唯一能猜出原名的线索。已知收窄：同毫秒撞名产生的
/// `1699-1-a.md` 会剥成 `1-a.md`——那个 `1` 是 `fileops::unique_trash_name`
/// 的序号，不是名字的一部分。这种条目只在"同毫秒删了两次同名文件**且**
/// 清单同时丢失"时出现，而那时名字是否完美已经不重要了（记在 BACKLOG）。
fn strip_stamp(entry: &str) -> String {
    match entry.split_once('-') {
        Some((head, rest))
            if !rest.is_empty() && head.parse::<i64>().map(|stamp| stamp > 0) == Ok(true) =>
        {
            rest.to_owned()
        }
        _ => entry.to_owned(),
    }
}

/// 从文件名前缀取删除时间。取不到返回 `None`（交给 mtime 兜底）。
fn stamp_of(name: &str) -> Option<i64> {
    let (head, _) = name.split_once('-')?;
    head.parse::<i64>().ok().filter(|stamp| *stamp > 0)
}

/// 文件 mtime → Unix 毫秒。取不到时 0：它只影响排序，不影响任何动作。
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// `rel` 这个名字现在有没有人占。
///
/// 用 `symlink_metadata` 而不是 `exists`：后者会跟随软链去问目标，
/// 而这里要问的是"这个名字有没有人占"（与 `unique_trash_name` 同一条理由）。
fn occupied(root: &Path, rel: &str) -> bool {
    std::fs::symlink_metadata(root.join(rel)).is_ok()
}

/// 撞名时的候选名：`c.md` → `c 2.md` → `c 3.md`。
///
/// 抽成纯函数（`taken` 由调用方注入）是为了让撞名分支能被**确定性**地测到：
/// 真实恢复里要造出"目标已存在"需要两步，而这条分支是唯一会覆盖别人文件的
/// 路径，值得单独钉住。
///
/// 序号插在扩展名**前面**（`c 2.md` 而不是 `c.md 2`）：后者会让文件不再是
/// Markdown，扫描器看不见它——恢复出来的东西反而从树里消失了。
///
/// 返回 `None` 表示候选全被占满（见 [`TARGET_SEQ_LIMIT`]）。
fn unique_target(path: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, path),
    };
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };

    for seq in 2..=TARGET_SEQ_LIMIT {
        let candidate = match ext {
            Some(ext) => format!("{stem} {seq}.{ext}"),
            None => format!("{stem} {seq}"),
        };
        let candidate = match dir {
            Some(dir) => format!("{dir}/{candidate}"),
            None => candidate,
        };
        if !taken(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 空目录：第一个候选就是 `名字 2.md`，序号在扩展名之前。
    #[test]
    fn a_collision_renames_to_space_two_before_the_extension() {
        assert_eq!(unique_target("a/b/c.md", |_| false).unwrap(), "a/b/c 2.md");
        // 目录部分必须原样保留：改名不能把文件搬出原目录。
        assert_eq!(unique_target("c.md", |_| false).unwrap(), "c 2.md");
    }

    /// 连续撞名：序号一路加，直到找到空位。
    #[test]
    fn consecutive_collisions_keep_counting_up() {
        let taken = |candidate: &str| matches!(candidate, "c.md" | "c 2.md" | "c 3.md");
        assert_eq!(unique_target("c.md", taken).unwrap(), "c 4.md");
    }

    /// 重试有上界：永远不会收敛的 `taken` 必须返回 `None`，不能死循环。
    #[test]
    fn an_always_taken_target_gives_up_instead_of_spinning() {
        assert!(unique_target("c.md", |_| true).is_none());
    }

    /// 多段扩展名：`a.tar.md` → `a.tar 2.md`，最后一段才是扫描器认的。
    #[test]
    fn multi_dot_names_keep_the_last_extension() {
        assert_eq!(unique_target("a.tar.md", |_| false).unwrap(), "a.tar 2.md");
    }

    /// 剥时间戳：正常条目、序号条目、以及"根本不是我们起的名字"。
    #[test]
    fn strip_stamp_recovers_the_original_name() {
        assert_eq!(strip_stamp("1700000000000-周会.md"), "周会.md");
        // 已知收窄：序号会留在名字里，见函数注释。
        assert_eq!(strip_stamp("1700000000000-1-周会.md"), "1-周会.md");
        // 不是我们的命名（用户手工放进来的）：原样返回，不猜。
        assert_eq!(strip_stamp("周会.md"), "周会.md");
        assert_eq!(strip_stamp("1700000000000-"), "1700000000000-");
    }

    /// 前缀解析：只有"纯数字 + 连字符"才算时间戳。
    #[test]
    fn stamp_of_only_accepts_a_numeric_prefix() {
        assert_eq!(stamp_of("1700000000000-a.md"), Some(1_700_000_000_000));
        assert_eq!(stamp_of("1700000000000-1-a.md"), Some(1_700_000_000_000));
        assert_eq!(stamp_of("a-1700.md"), None);
        assert_eq!(stamp_of("0-a.md"), None);
        assert_eq!(stamp_of("no-stamp"), None);
    }

    /// 路径闸：单个文件名通过，其余一律 `PathEscape`。
    #[test]
    fn check_entry_accepts_one_component_only() {
        assert!(check_entry("1700000000000-a.md").is_ok());
        assert!(check_entry("周会.md").is_ok());

        for bad in ["", ".", "..", "../a.md", "a/b.md", "a\\b.md", "/etc/passwd"] {
            let err = check_entry(bad).expect_err(bad);
            assert!(
                matches!(err, KiError::PathEscape { .. }),
                "{bad} 应当是 PathEscape，实际 {err:?}"
            );
        }
    }

    /// 一条记录在整次写入过程中的**任何一个瞬间**都必须是完整的 JSON。
    ///
    /// # 为什么不能靠 `kill -9` 验这件事
    ///
    /// 进程被杀不会丢页缓存：`fs::write` 写下去的字节照样在重启后读得到。
    /// 所以「删完立刻 kill -9 还能看到那条」验的是**落盘时机**，不是原子性——
    /// 换成 `fs::write` 它照样绿（`tests/trash.rs` 里那条就是这么写的，
    /// 它证明的是另一件事）。
    ///
    /// 真正的判据是并发读者：`tmp → fsync → rename` 让目标路径上的文件永远是
    /// 「旧的完整版」或「新的完整版」；而 `fs::write` 会先 `O_TRUNC`，
    /// 读者能读到 0 字节或半截 JSON。这就是本用例，也是 ADR-008 里那条
    /// 变异验证唯一会变红的用例。
    ///
    /// # 为什么反复重写同一条
    ///
    /// 读者需要一个稳定的路径。同一条记录被重写在真实使用里也会发生
    /// （恢复之后又删了同名文件），不是造出来的场景。
    ///
    /// 500 轮的依据：本机一次 `record()`（含 fsync）约 0.3–1ms，读者一次
    /// `fs::read` 约 10µs，于是每轮读者有几十次采样机会；截断窗口本身只有
    /// 几微秒，但 500 轮累计的采样次数足以稳定命中（实测见 ADR-008）。
    #[test]
    fn a_record_is_never_observed_half_written() {
        const WRITES: usize = 500;
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let entry = "1700000000000-a.md";
        let path = manifest_path(root, entry);

        // 先写一条，让读者从一开始就有东西可读。
        record(root, entry, "notes/a.md", 1_700_000_000_000).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let torn = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));

        let reader = {
            let stop = Arc::clone(&stop);
            let torn = Arc::clone(&torn);
            let reads = Arc::clone(&reads);
            let path = path.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            reads.fetch_add(1, Ordering::Relaxed);
                            // 完整记录的结构性特征：非空、以 `{` 开头、以 `}` 结尾。
                            // 0 字节（截断窗口）与半截 JSON 都在这里落网。
                            if bytes.is_empty()
                                || bytes.first() != Some(&b'{')
                                || bytes.last() != Some(&b'}')
                            {
                                torn.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        // 目标路径必须一直存在。
                        Err(err) if err.kind() == ErrorKind::NotFound => {
                            torn.fetch_add(1, Ordering::Relaxed);
                        }
                        // 共享冲突是瞬时的（Windows 上 rename 与 open 会撞），重试即可。
                        Err(_) => {}
                    }
                }
            })
        };

        for round in 0..WRITES {
            record(
                root,
                entry,
                &format!("notes/{round}.md"),
                1_700_000_000_000 + round as i64,
            )
            .unwrap();
        }

        stop.store(true, Ordering::Relaxed);
        reader.join().expect("读者线程 panic");

        assert!(
            reads.load(Ordering::Relaxed) > 0,
            "读者一次都没读到——用例没有验证任何东西"
        );
        assert_eq!(
            torn.load(Ordering::Relaxed),
            0,
            "读者观测到 {} 次半个文件（共 {} 次读）：清单写入不是原子的",
            torn.load(Ordering::Relaxed),
            reads.load(Ordering::Relaxed)
        );
    }
}
