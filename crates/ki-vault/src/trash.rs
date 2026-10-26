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
//!
//! 但**读侧不能按这个成本结构写**：10k 条的库上「枚举 + 逐条读清单」实测
//! 7.2 秒（release / 本机 NVMe，ADR-008），而面板是从侧栏底部点开的，用户
//! 对卡三秒毫无预期。所以列表是**分页**的（[`page`]）：排序键取自文件名
//! 前缀（不碰清单），一次调用最多读一页的清单。

use std::io::ErrorKind;
use std::path::{Component, Path};
use std::time::UNIX_EPOCH;

use ki_core::{
    Fallback, IoKind, KiError, PurgeResult, RestoreResult, TrashCursor, TrashEntry, TrashPage,
};
use serde::{Deserialize, Serialize};

use crate::fileops::move_no_clobber;
use crate::path::{
    ensure_parent_dir_usable, reject_metadata_path, resolve_for_write, validate_rel,
};
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

/// 读一条记录的三种结局。
///
/// # 为什么不是 `Option<Record>`
///
/// 列表只关心「有没有可信的 origin」，两种坏法对它是一回事；但**恢复**要把
/// 原因说出去：`RecordMissing`（没有这条记录）与 `RecordCorrupt`（记录在、
/// 读不出来）对用户是两句话，而旧代码把两者压成了一个 `None`。
enum RecordState {
    Ok(Record),
    /// 清单文件不存在——删除流程在「搬完文件、写清单之前」崩过，或者记录被外部删了。
    Missing,
    /// 文件在，但内容不能用：JSON 半截、字段缺失、或者根本读不出来（权限 / 路径被占）。
    Corrupt,
}

fn read_record(root: &Path, entry: &str) -> RecordState {
    match std::fs::read(manifest_path(root, entry)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(record) => RecordState::Ok(record),
            Err(_) => RecordState::Corrupt,
        },
        // 只有 NotFound 才是「没有这条记录」。其它读失败（权限、`trash-manifest`
        // 被一个同名文件占了）都是「记录不可用」——说成"没有记录"会指错方向。
        Err(err) if err.kind() == ErrorKind::NotFound => RecordState::Missing,
        Err(_) => RecordState::Corrupt,
    }
}

/// 记录里的原路径是否**可能**指向库内。
///
/// 复用 [`validate_rel`] 与 [`reject_metadata_path`]，不另写一份判断：
/// 清单是磁盘上的普通文件，用户（或某个坏掉的同步工具）可以改它。
/// 一条被改成 `../../.ssh/id_rsa` 的记录不该被显示成"原路径"，
/// 更不该被拿去恢复——真正的闸在 [`ensure_parent_dir_usable`] 与
/// [`resolve_for_write`]，这里只是**显示前**的过滤。
fn plausible_origin(origin: &str) -> Option<String> {
    if validate_rel(origin).is_err() || reject_metadata_path(origin).is_err() {
        return None;
    }
    Some(origin.to_owned())
}

/* ───────────────────── 列表（分页） ───────────────────── */

/// 一页默认多少条。前端不传或传 0 时用它。
pub const DEFAULT_PAGE_LIMIT: u32 = 200;

/// 一页最多多少条。前端传什么都越不过它。
///
/// 它是**服务端的上界**，不是产品参数：IPC 是公开边界，`trash_list(root,
/// null, 100000)` 若被放行，一次调用就会读 10 万个清单文件——分页等于没做。
/// 500 的依据是面板的可见高度（约 15 行）乘一个够用的滚动余量。
pub const MAX_PAGE_LIMIT: u32 = 500;

/// 排序与游标过滤的中间结构：只要名字与排序键，**不碰磁盘**。
#[derive(Debug, Clone)]
struct Candidate {
    entry: String,
    /// 排序键：文件名前缀里的 unix 毫秒；没有前缀就是 `0`（排最后）。
    stamp: i64,
}

fn candidate(entry: String) -> Candidate {
    let stamp = stamp_of(&entry).unwrap_or(0);
    Candidate { entry, stamp }
}

/// 在倒序里，`candidate` 是否严格排在 `cursor` 之后。
///
/// 倒序（最新在前）意味着「之后」= 键**严格小于**游标。相等的那一条就是
/// 游标自己，不能重复返回——这正是 tiebreaker 必须参与比较的原因。
fn is_after(candidate: &Candidate, cursor: &TrashCursor) -> bool {
    (candidate.stamp, candidate.entry.as_str()) < (cursor.deleted_at, cursor.entry.as_str())
}

/// 排序 → 游标过滤 → 多取一条。**纯函数**，`candidates` 由调用方注入。
///
/// # 为什么排序键是文件名前缀，而不是清单里的 `deletedAt`
///
/// `delete_note` 里 `stamp = unix_ms()` **同时**用于文件名
/// （`fileops.rs` 的 `unique_trash_name`）与清单 `deletedAt`（`record`），
/// 所以「按文件名前缀排序」与「按清单 `deletedAt` 排序」是**同一件事**，
/// 不是近似。这不是巧合，是那条赋值保证的。
///
/// 反过来做（先读清单再排序）会让分页不成立：为了排出第一页，得把 10k 条
/// 清单全读一遍，而那正是这一刀要消灭的 7 秒。**排序键必须不依赖 IO。**
///
/// 代价是清单被手改成别的 `deletedAt` 时，排序仍按文件名（显示值仍取清单）。
/// 这是**有意的**：宁可让一个被手改过的条目排在与它显示的时间不一致的位置，
/// 也不要为了它把整页的排序拖回 O(全量 IO)。
///
/// # `limit + 1`：`next` 是事实，不是估算
///
/// 多取的那一条**只**用来决定有没有下一页，绝不进结果。用
/// `selected.len() < limit` 判断会错两次：目录被跳过、条目在两次调用之间
/// 被恢复，都会让长度小于 `limit` 而其实还有下一页。
///
/// `limit` 必须 ≥ 1（由 [`clamp_limit`] 保证）。
fn select(
    candidates: Vec<Candidate>,
    cursor: Option<&TrashCursor>,
    limit: usize,
) -> (Vec<Candidate>, Option<TrashCursor>) {
    let mut sorted = candidates;
    // 倒序 + 条目名倒序。`stamp` 必须按 **i64 数值**比较：字典序里
    // `999-…` 大于 `1000-…`，而那会把新删的排到旧删的后面。
    sorted.sort_by(|a, b| b.stamp.cmp(&a.stamp).then_with(|| b.entry.cmp(&a.entry)));

    let mut rest: Vec<Candidate> = match cursor {
        None => sorted,
        Some(cursor) => sorted
            .into_iter()
            .filter(|candidate| is_after(candidate, cursor))
            .collect(),
    };

    let has_more = rest.len() > limit;
    rest.truncate(limit);
    let next = if has_more {
        rest.last().map(|last| TrashCursor {
            deleted_at: last.stamp,
            entry: last.entry.clone(),
        })
    } else {
        None
    };
    (rest, next)
}

/// `0 → 200`、`> 500 → 500`。服务端钳制，前端传什么都不能让一次调用读 10k 条清单。
fn clamp_limit(limit: u32) -> usize {
    match limit {
        0 => DEFAULT_PAGE_LIMIT as usize,
        n if n > MAX_PAGE_LIMIT => MAX_PAGE_LIMIT as usize,
        n => n as usize,
    }
}

/// 列出一页回收站，按删除时间倒序（最新在前）。
///
/// # 成本结构
///
/// 一次 `read_dir`（为了 `total` 与排序）+ **当前页那 ≤ limit 条**的
/// `symlink_metadata` 与清单读。排序发生在读清单**之前**，所以页的大小
/// 决定了 IO 量——这正是 10k 条从 7.24 秒降到 120 毫秒的原因（ADR-008 实测）。
///
/// 「是不是文件」用 `DirEntry::file_type()`：Windows 上它来自目录枚举本身，
/// 不额外 syscall。`metadata()` 只对当前页做——`size` 与 `mtime` 都只服务
/// 展示（`mtime` 仅在没有时间戳前缀时兜底），没理由为看不见的行付这笔钱。
///
/// # 顺序与判据
///
/// - **一条回收站记录 = 回收目录里的一个普通文件**。清单只提供附加信息，
///   不参与"有没有这一条"的判定。
/// - 排序键是文件名前缀（见 [`select`]）；没有前缀的条目（用户手工放进
///   回收目录的）按 `0` 处理，排在最后，仍然能列出、仍然能恢复。
/// - `deleted_at` 三级回退：清单 → 文件名前缀 → 文件 mtime。三级都失败才用 0。
/// - `origin` 为 `None` 是**正常状态**，不是错误：清单缺失、损坏、或记录被
///   改成了越界路径都会落到这里。UI 必须把它显示成「原路径未知」。
///
/// # 单个条目失败不连坐
///
/// 读不出元数据的条目直接跳过（与扫描器同一条策略）：一个被占用的文件
/// 不该让整个回收站打不开。它仍计入 `total`——`total` 是一次 `read_dir`
/// 的快照，下次刷新会自动对齐。
pub fn page(root: &Path, cursor: Option<&TrashCursor>, limit: u32) -> Result<TrashPage, KiError> {
    let limit = clamp_limit(limit);
    let dir = root.join(TRASH_DIR);
    let listing = match std::fs::read_dir(&dir) {
        Ok(listing) => listing,
        // 没有回收目录 = 回收站是空的。这不是错误：从没删过东西的库就是这样。
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Ok(TrashPage {
                entries: Vec::new(),
                total: 0,
                next: None,
            })
        }
        Err(err) => return Err(KiError::io(TRASH_DIR, &err)),
    };

    let mut candidates = Vec::new();
    for entry in listing {
        let Ok(entry) = entry else { continue };
        // 目录与软链不是"被删掉的笔记"。回收目录是平的，出现目录只可能是
        // 用户自己放进去的，把它列出来再让人"恢复"会搬出一个目录树。
        // `file_type()` 读不出来时同样跳过：连是不是文件都不知道，不能算进 total。
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        candidates.push(candidate(entry.file_name().to_string_lossy().into_owned()));
    }

    let total = candidates.len() as u64;
    let (selected, next) = select(candidates, cursor, limit);

    let mut entries = Vec::with_capacity(selected.len());
    for candidate in selected {
        // 只有这一页的条目才碰 IO。枚举之后被外部删掉的，跳过。
        let Ok(meta) = std::fs::symlink_metadata(dir.join(&candidate.entry)) else {
            continue;
        };

        // 列表只关心「有没有可信的 origin」：记录缺失与记录损坏在这里是
        // 同一件事（都显示成「原路径未知」）。恢复才需要区分它们。
        let record = match read_record(root, &candidate.entry) {
            RecordState::Ok(record) => Some(record),
            RecordState::Missing | RecordState::Corrupt => None,
        };
        let deleted_at = record
            .as_ref()
            .map(|record| record.deleted_at)
            .filter(|stamp| *stamp > 0)
            .or_else(|| stamp_of(&candidate.entry))
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
            .unwrap_or_else(|| strip_stamp(&candidate.entry));

        entries.push(TrashEntry {
            entry: candidate.entry,
            name: display,
            origin,
            deleted_at,
            size: meta.len(),
        });
    }

    Ok(TrashPage {
        entries,
        total,
        next,
    })
}

/* ───────────────────── 恢复 ───────────────────── */

/// 清单能给这次恢复提供什么。
///
/// 两种结局，对应两种完全不同的起点：记录可信（知道它原来在哪），或者不可信
/// （只能从文件名猜）。把"为什么不可信"一起带出来，前端才能说清是哪一种。
enum Intent {
    Origin(String),
    Guess(Fallback),
}

/// 读清单，判定这次恢复的**意图**。
fn intent(root: &Path, entry: &str) -> Intent {
    match read_record(root, entry) {
        RecordState::Ok(record) => match plausible_origin(&record.origin) {
            Some(origin) => Intent::Origin(origin),
            // 记录在，但里面的路径不可信（被改成 `../../` 之类）。这不是
            // "没有记录"，是"记录不可用"——两种事实，两句文案。
            None => Intent::Guess(Fallback::RecordCorrupt),
        },
        RecordState::Missing => Intent::Guess(Fallback::RecordMissing),
        RecordState::Corrupt => Intent::Guess(Fallback::RecordCorrupt),
    }
}

/// 降级判据：**只有「父目录不存在」才降级**，其余错误一律冒泡。
///
/// # 为什么单独抽成函数
///
/// 这是整条恢复路径上唯一一个"把失败翻译成另一种结果"的地方，也是唯一
/// 能悄悄把用户的文件搬到别处的地方。抽出来之后它可以用**构造出来的错误**
/// 直接测（不需要真的把目录设成只读），于是这条判据在三个平台上都验得到。
///
/// # 为什么不是「任何错误都降级」
///
/// 降级是替用户做决定：把文件放到一个他没要求的地方。它只在"原来的位置已经
/// 不可能回去了"时成立。只读、权限不足、路径上有重解析点都是"现在回不去，
/// 但改一下就能回去"——用户要的是去改权限，不是被悄悄搬家。
///
/// 判据是**结构化**的 `IoKind::NotFound`，不是错误文案：match 字符串等于把
/// 判据藏进提示语，改一句话就静默改了行为，而且没有任何测试会红。
fn fallback_for(err: &KiError) -> Option<Fallback> {
    match err {
        KiError::Io {
            kind: IoKind::NotFound,
            ..
        } => Some(Fallback::OriginGone),
        _ => None,
    }
}

/// 把回收目录里的 `entry` 搬回原目录，返回它最终落在哪里。
///
/// # 落点决策
///
/// | 磁盘上的事实 | 落到 | `fallback` |
/// |---|---|---|
/// | 清单有记录，原目录还在 | 原路径 | `null` |
/// | 清单有记录，**原目录没了** | 库根 + 剥出的原名 | `originGone` |
/// | 清单里没有这条记录 | 库根 + 剥出的原名 | `recordMissing` |
/// | 清单有记录但读不出来 | 库根 + 剥出的原名 | `recordCorrupt` |
/// | 目标已存在（文件或目录） | 原目录里的 `名字 2.md` | 不变，`renamed` 为真 |
/// | 父目录只读 / 权限不足 / 路径上有重解析点 | **不搬**，返回 `KiError` | — |
///
/// # 为什么只有「原目录不存在」才降级
///
/// 降级是**替用户做决定**：把文件放到一个他没要求的地方。它只在一件事上成立
/// ——原来的位置已经不可能回去了（目录没了）。其余失败都是"现在回不去，但
/// 改一下就能回去"：
///
/// - 父目录只读 / 权限不足 → 用户去改权限，文件还在回收站里等着；
/// - 原路径上有个同名目录 → **不是失败**，在原目录里改个名就行（`c 2.md`）。
///
/// 旧实现用 `resolve_for_write(...).ok()` 把**所有**错误都当成"原目录不可用"，
/// 于是这两种情况都静默落到了库根：用户以为恢复到了原处，文件其实在别的地方。
///
/// **不代建原目录**：删除只搬文件，目录本来就还在；目录没了说明是外部删的，
/// 这时悄悄重建一串目录比"落到根目录并明确告知"更容易让用户困惑（ADR-008）。
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

    // 1. 想回到哪（意图），以及落在哪（事实）。两者在降级时分叉。
    let (requested, base, fallback) = match intent(&canonical_root, entry) {
        Intent::Origin(origin) => match ensure_parent_dir_usable(&canonical_root, &origin) {
            // 原目录还在：意图与落点都是它。
            Ok(()) => (origin.clone(), origin, None),
            Err(err) => match fallback_for(&err) {
                Some(reason) => (origin, strip_stamp(entry), Some(reason)),
                None => return Err(err),
            },
        },
        Intent::Guess(reason) => {
            let name = strip_stamp(entry);
            (name.clone(), name, Some(reason))
        }
    };

    // 2. 撞名就换名字，绝不覆盖。`renamed` 只由这一步置位——不能拿
    //    `path != requested` 当判据：`originGone` 下 requested 是意图
    //    （`a/b/c.md`）、path 是事实（`c.md`），两者天然不同。
    let (path, renamed) = land(&base, |candidate| occupied(&canonical_root, candidate))
        .ok_or_else(|| {
            KiError::Internal(format!(
                "目标目录里已有 {TARGET_SEQ_LIMIT} 个同名文件，已放弃恢复：{base}"
            ))
        })?;

    // 3. 搬。`move_no_clobber` 会再挡一次"检查之后目标才出现"的竞态。
    let target = resolve_for_write(&canonical_root, &path)?;
    move_no_clobber(&source, &target, &path)?;

    // 4. 文件已经回去了，才删清单。反过来的话，崩溃会留下"清单说有、
    //    文件没有"的悬空记录——那比丢一条元数据更糟。
    forget(&canonical_root, entry);

    Ok(RestoreResult {
        path,
        requested,
        fallback,
        renamed,
        // 索引不归这一层管（`ki-vault` 不认识 SQLite）。`trash_restore`
        // 补完索引再把这一位翻成真——在那之前，"索引里没有它"是事实。
        indexed: false,
    })
}

/* ───────────────────── 清空 ───────────────────── */

/// 清空回收站，返回「删掉了几个、还剩几个」。
///
/// # 只删文件
///
/// 回收目录是平的，我们只往里放文件。用户若自己往里放了一个目录，
/// **不递归删**——那不是我们造的东西，也不在"清空回收站"的承诺里。
/// 跳过比递归安全：递归删错一次的代价是不可逆的。
///
/// # 清单按条清，不能整块删
///
/// 清单是 `trash/` 的兄弟目录（见 [`MANIFEST_DIR`]）。整块 `remove_dir_all`
/// 会连带删掉**删不掉那几篇**的记录——文件还在回收目录里，原路径却没了，
/// 列表上那一条当场退化成「原路径未知」。而这一刀刚刚答应用户
/// 「M 篇删不掉，已留在列表里」：留一个不知道自己原来在哪的条目，等于把
/// 那句承诺打了个折。
///
/// 所以只删「对应文件已经不在回收目录里」的记录。判据用
/// [`still_in_trash`]：**只有确定不存在才删记录**，读不出来（被占用）时
/// 宁可留着——孤儿记录无害，丢原路径有害。
///
/// # 部分失败也要把话说完
///
/// 一个文件被别的程序占住时，其余条目照删。旧实现返回第一个 `Err` 并**丢掉
/// `removed`**：前端拿到错误就 return，既不刷新列表也不说删了几个，于是面板
/// 上还列着已经被删掉的文件——一个面板，两个真相。
///
/// 现在它**永远 `Ok`**（除非回收目录本身读不出来——那时一个都没动，是真的
/// 失败）。删不掉的计入 `failed`，它们仍然躺在回收目录里，刷新后照旧看得见。
pub fn purge(root: &Path) -> Result<PurgeResult, KiError> {
    let dir = root.join(TRASH_DIR);
    let mut removed = 0u64;
    let mut failed = 0u64;

    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                // 连目录项都读不出来：既不能删也不知道是什么。算作"删不掉"
                // 比静默跳过诚实——后者会让用户以为清空彻底成功了。
                let Ok(entry) = entry else {
                    failed += 1;
                    continue;
                };

                // 目录不是我们造的，跳过（也不算失败：它本来就不在承诺里）。
                if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    continue;
                }

                match std::fs::remove_file(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(_) => failed += 1,
                }
            }
        }
        // 没有回收目录 = 没有东西可清。清单目录可能还在（比如只删过一次又
        // 手工挪走了文件），所以这里不 early return，继续去清清单。
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(KiError::io(TRASH_DIR, &err)),
    }

    sweep_manifest(root);

    Ok(PurgeResult { removed, failed })
}

/// 这个条目**确定**已经不在回收目录里了吗？
///
/// 只有 `NotFound` 才算"不在"。读不出来（被占用、权限）一律当成"还在"：
/// 宁可留一条孤儿记录（读侧本来就会忽略），也不要丢掉一份原路径。
fn still_in_trash(root: &Path, entry: &str) -> bool {
    !matches!(
        std::fs::symlink_metadata(root.join(TRASH_DIR).join(entry)),
        Err(err) if err.kind() == ErrorKind::NotFound
    )
}

/// 清掉「文件已经不在」的清单记录；目录空了才把目录删掉。
///
/// 失败只写 stderr：清空的主体（抹掉文件）已经完成，剩下的孤儿记录无害。
fn sweep_manifest(root: &Path) {
    let dir = manifest_dir(root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    let mut left = 0usize;
    for entry in entries {
        let Ok(entry) = entry else {
            left += 1;
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(entry_name) = name.strip_suffix(".json") else {
            // 不是我们的命名：不碰它，也算作"还有东西"，别删目录。
            left += 1;
            continue;
        };
        if still_in_trash(root, entry_name) {
            left += 1;
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }

    // 空目录才删。`remove_dir` 只删空目录，正好；非空是正常状态（还有删不掉的
    // 条目靠这些记录记住原路径），不报错也不重试。
    if left == 0 {
        if let Err(err) = std::fs::remove_dir(&dir) {
            eprintln!("回收站清单目录清理失败（残留的孤儿记录无害，下次清空会再试）：{err}");
        }
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

/// 决定最终落在哪，以及**是否真的改了名**。
///
/// # 为什么把这两件事绑在一起返回
///
/// `renamed` 的判据只有一个：`unique_target` 有没有被调用过。把它写成
/// `path != requested` 是错的——`originGone` 下 `requested` 是意图
/// （`a/b/c.md`）、`path` 是事实（`c.md`），两者天然不同，那样写会把
/// "降级到库根"说成"撞名改名"。
///
/// 绑成一个函数之后，调用方拿不到 `path` 与 `renamed` 的不一致组合：
/// 要么没改名（原样返回），要么改了名（`true`）。
///
/// 返回 `None` 表示候选全被占满（见 [`TARGET_SEQ_LIMIT`]）。
fn land(base: &str, taken: impl Fn(&str) -> bool) -> Option<(String, bool)> {
    if !taken(base) {
        return Some((base.to_owned(), false));
    }
    unique_target(base, taken).map(|path| (path, true))
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
        assert!(land("c.md", |_| true).is_none());
    }

    /// `renamed` 的判据是「`unique_target` 有没有被调用过」，不是
    /// `path != requested`。
    ///
    /// # 这条用例钉住的是什么
    ///
    /// 降级落到库根时，`requested`（意图）是 `a/b/c.md`、`path`（事实）是
    /// `c.md`——两者不同，但**没有改名**。若有人把判据改回
    /// `path != requested`，`OriginGone` 就会被报成「撞名改名」，用户会以为
    /// 根目录里有一份同名文件跟他抢名字。
    ///
    /// 真实恢复里造这个组合要三步（删笔记 → 删目录 → 恢复），所以这里用
    /// 注入 `taken` 的纯函数把它钉死：`land` 的第二个返回值就是 `renamed`。
    #[test]
    fn a_fallback_landing_is_not_a_rename() {
        // 没撞名：原样落，没改名。
        assert_eq!(land("c.md", |_| false), Some(("c.md".to_owned(), false)));
        // 撞名：换名字，`renamed` 为真。
        assert_eq!(
            land("c.md", |candidate| candidate == "c.md"),
            Some(("c 2.md".to_owned(), true))
        );
        // 落点是库根的名字（降级后的 base）时，改名也只发生在那个目录里。
        assert_eq!(
            land("c.md", |candidate| candidate == "c.md"
                || candidate == "c 2.md"),
            Some(("c 3.md".to_owned(), true))
        );
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

    /* ───────────── 分页：排序与游标（纯函数） ───────────── */

    /// 把一组名字排成「一次调用该返回的那一页」。
    fn page_of(names: &[&str], cursor: Option<&TrashCursor>, limit: usize) -> Vec<String> {
        let candidates = names
            .iter()
            .map(|name| candidate((*name).to_owned()))
            .collect();
        let (selected, _) = select(candidates, cursor, limit);
        selected.into_iter().map(|item| item.entry).collect()
    }

    /// 一路用 `next` 翻到底，返回按顺序拼起来的条目名。
    fn walk(names: &[&str], limit: usize) -> Vec<String> {
        let mut all = Vec::new();
        let mut cursor: Option<TrashCursor> = None;
        for _ in 0..=names.len() {
            let candidates: Vec<Candidate> =
                names.iter().map(|n| candidate((*n).to_owned())).collect();
            let (selected, next) = select(candidates, cursor.as_ref(), limit);
            all.extend(selected.into_iter().map(|item| item.entry));
            match next {
                Some(next) => cursor = Some(next),
                None => return all,
            }
        }
        panic!("翻页没有收敛：游标一直在返回 Some");
    }

    /// 排序键必须按 **i64 数值**比较，不是字符串。
    ///
    /// # 这条用例钉住的是什么
    ///
    /// 文件名前缀是十进制数字，而字典序里 `"999"` > `"1000"`。把
    /// `sort_by(|a, b| b.entry.cmp(&a.entry))` 当成排序实现（文件名整体比较）
    /// 会把 999ms 的旧条目排到 1000ms 的新条目前面。真实时间戳都是 13 位，
    /// 平时看不出差别——只有跨位数（或人为造的短前缀）才会红，所以必须
    /// 显式造这两条。
    ///
    /// 变异：把 `select` 里的 `b.stamp.cmp(&a.stamp)` 换成
    /// `b.entry.cmp(&a.entry)`，这条会红。
    #[test]
    fn the_sort_key_is_the_numeric_prefix_not_the_string() {
        let ordered = page_of(&["999-old.md", "1000-new.md"], None, 10);
        assert_eq!(ordered, vec!["1000-new.md", "999-old.md"]);
    }

    /// 同一毫秒的两条：tiebreaker 是 `entry`，跨页各出现一次。
    ///
    /// 页大小 1 时，边界正好落在两条同毫秒条目之间。没有 tiebreaker（只用
    /// `stamp` 比较）的实现会在第二页把另一条一起排除（`>` 判据）或重复
    /// 返回同一条（`>=` 判据）。
    #[test]
    fn same_millisecond_entries_cross_a_page_boundary_once() {
        let names = [
            "1700000000000-a.md",
            "1700000000000-b.md",
            "1700000000001-c.md",
        ];
        assert_eq!(walk(&names, 1), vec![names[2], names[1], names[0]]);
    }

    /// 展示名相同、真名不同：游标必须认 `entry`。
    ///
    /// `a/c.md` 与 `b/c.md` 的展示名都是 `c.md`。若游标用展示名，两条会
    /// 互相覆盖，翻页直接漏掉一条。
    #[test]
    fn the_cursor_uses_the_entry_name_not_the_display_name() {
        let names = ["1700000000000-a-c.md", "1700000000000-b-c.md"];
        assert_eq!(walk(&names, 1).len(), 2);
        assert_eq!(walk(&names, 1), vec![names[1], names[0]]);
    }

    /// 没有时间戳前缀的条目（用户手工放进回收目录）：按 0 排最后，仍列出。
    #[test]
    fn entries_without_a_stamp_sort_last_but_are_still_listed() {
        let names = ["hand-placed.md", "1700000000000-real.md"];
        assert_eq!(page_of(&names, None, 10), vec![names[1], names[0]]);
        // 翻页也翻得到它：不能被"排序键为 0"吞掉。
        assert_eq!(walk(&names, 1), vec![names[1], names[0]]);
    }

    /// 翻页不重不漏：集合相等，且去重后条数不变。
    #[test]
    fn walking_every_page_yields_each_entry_exactly_once() {
        let names: Vec<String> = (0..97)
            .map(|index| format!("{}-n{index:03}.md", 1_700_000_000_000i64 + index))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        let walked = walk(&refs, 7);
        assert_eq!(walked.len(), names.len(), "翻页漏条或重复");

        let mut unique = walked.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "翻页返回了重复条目");

        // 顺序也必须是全序的倒序：一趟翻页拼起来 == 一次全量排序。
        let mut expected = names.clone();
        expected.reverse();
        assert_eq!(walked, expected);
    }

    /// `next` 是事实：正好 `limit` 条时它必须是 `null`。
    ///
    /// 这条挡住「拿 `entries.len() == limit` 当还有下一页」的写法——那会让
    /// 前端多请求一次空页，然后按钮永远停在「加载更多」。
    #[test]
    fn next_is_null_when_the_last_page_is_exactly_full() {
        let names: Vec<String> = (0..6)
            .map(|index| format!("{}-n{index}.md", 1_700_000_000_000i64 + index))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let candidates = || {
            refs.iter()
                .map(|name| candidate((*name).to_owned()))
                .collect::<Vec<_>>()
        };

        let (selected, next) = select(candidates(), None, 6);
        assert_eq!(selected.len(), 6);
        assert!(next.is_none(), "正好一页时不该有下一页");

        let (selected, next) = select(candidates(), None, 5);
        assert_eq!(selected.len(), 5);
        let next = next.expect("还有一条没读，必须有下一页");
        assert_eq!(next.entry, selected[4].entry, "游标指向本页最后一条");
        assert_eq!(next.deleted_at, selected[4].stamp);
    }

    /// `limit` 服务端钳制：前端传什么都不能让一次调用读 10k 条清单。
    #[test]
    fn the_limit_is_clamped_by_the_server() {
        assert_eq!(clamp_limit(0), DEFAULT_PAGE_LIMIT as usize);
        assert_eq!(clamp_limit(1), 1);
        assert_eq!(clamp_limit(200), 200);
        assert_eq!(clamp_limit(MAX_PAGE_LIMIT), MAX_PAGE_LIMIT as usize);
        assert_eq!(clamp_limit(MAX_PAGE_LIMIT + 1), MAX_PAGE_LIMIT as usize);
        assert_eq!(clamp_limit(u32::MAX), MAX_PAGE_LIMIT as usize);
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

    /// 降级判据：**只有「父目录不存在」才降级**，其余错误一律冒泡。
    ///
    /// # 这条用例为什么用构造出来的错误
    ///
    /// 真实的"只读 / 权限不足 / 路径上有链接"在三个平台上造法各不相同，
    /// 而 Windows 上能造出共享冲突、Unix 上能造出 EACCES 的写法互不通用。
    /// 判据本身是纯函数，直接喂错误码就能钉死；端到端行为由
    /// `tests/trash.rs` 的三条集成用例覆盖（Unix 两条 + 链接一条）。
    ///
    /// # 变异验证
    ///
    /// 把 `fallback_for` 改成「任何错误都返回 `OriginGone`」，这条会红：
    /// 后三个断言会拿到 `Some(OriginGone)` 而不是 `None`。
    #[test]
    fn only_a_missing_directory_degrades() {
        let io = |kind: ErrorKind| KiError::io("a/b", &std::io::Error::from(kind));

        // 目录真的没了：这是唯一该降级的情况。
        assert_eq!(
            fallback_for(&io(ErrorKind::NotFound)),
            Some(Fallback::OriginGone)
        );
        // 只读 / 权限不足：用户去改权限，不是我们替他搬家。
        assert_eq!(fallback_for(&io(ErrorKind::PermissionDenied)), None);
        // 被占用（Windows 的共享冲突）：等一下再试，不是换个地方放。
        assert_eq!(fallback_for(&io(ErrorKind::WouldBlock)), None);
        // 路径上有重解析点：写路径的边界，恢复不能绕过它。
        assert_eq!(
            fallback_for(&KiError::PathEscape {
                path: "a/b".to_owned()
            }),
            None
        );
        // 父目录的位置被一个同名文件占了：不是"目录没了"，改个名就能回去。
        assert_eq!(
            fallback_for(&KiError::NotAFile {
                path: "a/b".to_owned()
            }),
            None
        );
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
