//! Vault 扫描器。
//!
//! 设计要点（每一条都对应一个真实会踩的坑）：
//!
//! - **剪枝而不是过滤**：命中 `.git` 时直接不下降（`filter_entry`），
//!   而不是遍历完再丢弃。在一个带 `.git` 的 Vault 上这是 10 倍差距。
//!   代价是「被剪掉的子树里有多少篇笔记」这个数**不存在**——我们没访问过它。
//!   所以忽略永远不进 `skipped`（ADR-014）。
//! - **忽略名单可配置**：内置名单 + 用户配置（`.knowisle/config.toml`）。
//!   配置在 [`scan_vault`] 开头读**一次**，编译成 [`ignore::Matcher`] 之后
//!   才进 `filter_entry`——绝不每个目录读一次文件。
//! - **不跟随符号链接**：否则一个指向父目录的软链会让遍历无限递归。
//! - **错误只跳过不传播**：单个文件权限不足不该让整次扫描失败。
//! - **路径一律 POSIX**：见 `crate::path` 的说明。
//! - **非 UTF-8 文件名跳过**：绝不 `unwrap()`，也绝不塞替换字符。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path};
use std::time::{Instant, UNIX_EPOCH};

use walkdir::{DirEntry, WalkDir};

use ki_core::ignore::IgnoreRules;
use ki_core::skip::{ScanSkipEntry, ScanSkipReason};
use ki_core::{FileNode, KiError, NoteContent, ScanResult};

use crate::encoding;
use crate::ignore;
use crate::path::{to_posix, ResolvedVault};

/// 纳入笔记树的扩展名（大小写不敏感）。
const NOTE_EXTENSIONS: &[&str] = &["md", "markdown"];

/// 单篇笔记读取上限。超过就拒绝，避免用户误点一个 2GB 的假 `.md` 把内存打爆。
pub(crate) const MAX_NOTE_BYTES: u64 = 10 * 1024 * 1024;

/// 跳过清单最多返回多少条。
///
/// # 为什么是 50 而不是 1000
///
/// 这是**诊断**清单，不是待办清单：用户要的行动是「把这个文件挪走 / 改个名 /
/// 删掉」，而正常库里被跳过的条目是个位数。50 条足够覆盖「误选了整个用户
/// 目录」之外的绝大多数真实情况，同时保证这一次扫描的额外分配与序列化开销
/// 与树的大小无关（10k 库实测增量在噪声内，见 `tests/skip_gate.rs`）。
///
/// 超过时 `skipped_truncated` 为真，UI 说「只列出前 50 条」，
/// 而 `skipped` 仍然是精确总数——契约照抄 ADR-012（total 精确、truncated
/// 只描述清单），不许重新发明一套。
pub const SKIP_LIST_MAX: usize = 50;

/// 跳过条目的收集器。
///
/// # 为什么是一个结构体而不是散在循环里的 `skipped += 1`
///
/// 五处跳过的**原因互不相同**，而它们此前共用同一个整数。收成一个类型之后，
/// 有三条不变量只能在这一个地方维持，也就只可能被破坏一次：
///
/// 1. `total` 精确（含不进清单的 `Internal`）；
/// 2. 清单长度 ≤ [`SKIP_LIST_MAX`]；
/// 3. `truncated` 只由「**本来可以进清单**的条数」决定——不能拿
///    `total > entries.len()` 推，因为 `Internal` 从来不进清单，那样推会把
///    「有 1 条 Internal」误报成「清单被截断」。
#[derive(Debug, Default)]
struct SkipLog {
    /// 精确总数：所有原因之和。
    total: usize,
    /// 本来可以进清单的条数（不含 `Internal`）。`truncated` 只由它决定。
    listable: usize,
    entries: Vec<ScanSkipEntry>,
    truncated: bool,
}

impl SkipLog {
    /// 记一条**用户修不了**的跳过：只进总数，不进清单。
    ///
    /// 目前只有 [`ScanSkipReason::Internal`] 走这里。把这类条目列给用户看，
    /// 等于告诉他「这里有件事你能处理」，而他处理不了——那比不说更糟。
    fn internal(&mut self) {
        self.total += 1;
    }

    /// 记一条可以定位的跳过。
    ///
    /// `Internal` 会被挡在清单外（红线：用户修不了的只进总数），所以调用方
    /// 不必自己判断——判断只有这一处，漏不掉。
    fn record(&mut self, entry: ScanSkipEntry) {
        self.total += 1;
        if entry.reason == ScanSkipReason::Internal {
            return;
        }
        self.listable += 1;
        if self.entries.len() < SKIP_LIST_MAX {
            self.entries.push(entry);
        } else {
            self.truncated = true;
        }
    }
}

/// 把相对路径转成「清单里能显示的一行」。
///
/// 返回 `(POSIX 路径, 是否 lossy)`。
///
/// [`to_posix`] 返回 `None` 时退回逐段 `to_string_lossy`——清单里必须有一行
/// 看得见的东西，但调用方要据此加上「名字含无法显示的字符」的标记：
/// `U+FFFD` 冒充真名字，用户照着它去改名会找不到文件（红线）。
fn posix_lossy(relative: &Path) -> (String, bool) {
    if let Some(path) = to_posix(relative) {
        return (path, false);
    }
    let mut parts: Vec<String> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(segment) => parts.push(segment.to_string_lossy().into_owned()),
            Component::CurDir => {}
            // RootDir / Prefix / ParentDir 在 `strip_prefix` 之后不该出现。
            // 出现了也不能 panic：这一条本来就是要报给用户的那一类。
            _ => {}
        }
    }
    (parts.join("/"), true)
}

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

/// 忽略规则的来源。
///
/// # 为什么是枚举而不是一个 `bool read_config`
///
/// 调用点读得懂：`ignore: BuiltinOnly` 说的是「我要一份**可复现的基线**」，
/// 而 `read_config: false` 只说了「不要读」。将来要加「从指定路径读」
/// （`isle scan --config <path>`）时只多一个变体，不用再长第二个布尔，
/// 也不用重新解释两个布尔同时为真是什么意思。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IgnoreSource {
    /// 只用内置名单，**不碰磁盘上的配置**。
    ///
    /// 测试与「我要知道默认名单到底忽略了什么」的场景用它。它等价于
    /// 改动之前的行为——判据 1 的「逐字节相同」就是拿它与带配置的扫描对比。
    BuiltinOnly,
    /// 读 `<vault>/.knowisle/config.toml`（缺文件 = 只用内置）。
    ///
    /// 应用走这条。配置写坏不会让扫描失败，只会在 `ScanResult.ignore_rules`
    /// 的 `config_error` 里留下一句人话（ADR-014）。
    #[default]
    VaultConfig,
}

/// 扫描参数。默认值对 99% 的 Vault 都够用。
///
/// 唯一不能随意调大的是 [`ScanOptions::max_depth`]：它是请求值，会被收敛。
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// 条目数上限（**目录 + 文件**），安全阀。
    ///
    /// # 它数的不是笔记
    ///
    /// [`RawEntry`] 的扁平表同时装目录与文件，所以撞上限时能看见的笔记数
    /// **不超过**上限（只有「全部笔记都在库根、没有任何子目录」时才恰好相等）。
    /// ADR-015 门禁①在 10k 夹具上实测：
    ///
    /// | 夹具形状 | entries/notes | 撞 20000 条目时能看见 |
    /// |---|---|---|
    /// | 10k 篇 / 50 目录（平铺） | 1.0050 | **19900 篇** |
    /// | 10k 篇 / 499 目录 / 5 层 | 1.0499 | **19049 篇** |
    ///
    /// 所以「两万篇」这个承诺差的是 0.5%–4.75%，不是十倍。**这不是业务
    /// 上限**：它防的是「误选了整个用户目录」把 UI 卡死。
    ///
    /// # 为什么没有 `max_notes`
    ///
    /// 想给「最多索引多少篇」做产品承诺的人，必须先回答「截断时索引怎么
    /// 安全剪枝」：`truncated` 是索引侧的「别删」信号，业务上限一旦让它
    /// 常态为真，磁盘上删掉的笔记会**永久**留在索引里——门禁③实测
    /// 100 篇一条都清不掉。见 ADR-015。
    pub max_entries: usize,
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
    /// 忽略规则从哪来。
    ///
    /// **不加一个 `rules: IgnoreRules` 字段**，理由有两条：
    /// ① 那样每个调用方（含 20 多个测试）都要自己先读一次配置，而读配置
    /// 属于扫描的前置条件，不该外泄给调用方；② 规则里带着 `config_error`，
    /// 让调用方构造它就等于允许调用方伪造一条「配置错误」。
    pub ignore: IgnoreSource,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_entries: 20_000,
            max_depth: 32,
            follow_links: false,
            prune_empty_dirs: false,
            ignore: IgnoreSource::default(),
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
    let mut skips = SkipLog::default();
    let mut truncated = false;

    // 唯一收敛点：无论调用方传 32 还是 usize::MAX，树深都不会超过 MAX_SAFE_DEPTH。
    // 这一行就是"栈安全不依赖入参"的全部实现，别把它删掉或挪到别处。
    let depth_limit = clamp_depth(opts.max_depth);

    // 配置**只在这里读一次**（红线）：`filter_entry` 每个目录项都会被调用，
    // 在里面读文件等于把一次 1 KB 的 read 乘上条目数。
    let rules = match opts.ignore {
        IgnoreSource::BuiltinOnly => IgnoreRules::builtin_only(),
        IgnoreSource::VaultConfig => ignore::load(&root_abs),
    };
    let matcher = ignore::Matcher::new(&rules);

    let walker = WalkDir::new(&root_abs)
        .follow_links(opts.follow_links)
        .max_depth(depth_limit)
        // 确定性输出：同一份 Vault 每次扫描得到完全相同的顺序，CI 才能断言。
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| should_descend(entry, &matcher));

    for item in walker {
        let entry = match item {
            Ok(entry) => entry,
            // 权限不足、遍历中途文件消失、目录被并发删除：跳过这一个，继续走。
            //
            // 这里**没有**"软链成环"这种情况：`follow_links(false)` 下 WalkDir
            // 根本不进入软链，也就不会形成环。排查问题时别被这个念头带偏。
            //
            // # 两种原因，靠 `io_error()` 分
            //
            // - `PermissionDenied` → 用户能修（改权限 / 换个地方放）；
            // - 其余（含 `io_error()` 为 `None` 的 `Loop`）→ 用户多半修不了，
            //   但仍然给路径：文件被删了这件事，说出来比不说好。
            //
            // 路径同样可能拿不到（`Loop` 变体只给祖先与子节点），所以是
            // `Option`——清单里必须能渲染「没有路径」。
            Err(err) => {
                let reason = match err.io_error().map(std::io::Error::kind) {
                    Some(std::io::ErrorKind::PermissionDenied) => ScanSkipReason::Permission,
                    _ => ScanSkipReason::WalkError,
                };
                let (path, path_lossy) = match err.path() {
                    Some(failed) => match failed.strip_prefix(&root_abs) {
                        Ok(relative) => {
                            let (path, lossy) = posix_lossy(relative);
                            (Some(path), lossy)
                        }
                        // 报错的路径不在库根之下（理论上不可达）。给绝对路径
                        // 等于把库外的路径塞进「Vault 相对路径」这个字段，
                        // 前端拼面包屑时会拼出一堆 `..`。宁可说「没有路径」。
                        Err(_) => (None, false),
                    },
                    None => (None, false),
                };
                skips.record(ScanSkipEntry {
                    path,
                    path_lossy,
                    reason,
                });
                continue;
            }
        };

        if entry.depth() == 0 {
            continue; // 根目录自身不入树
        }

        let relative = match entry.path().strip_prefix(&root_abs) {
            Ok(relative) => relative,
            // 走的是 `WalkDir::new(&root_abs)`，所以每一个产出的路径都必然
            // 以它开头——这一支**不可达**。真到了这里说明 walkdir 的行为变了，
            // 那是我们自己的 bug，不是用户能修的问题：只进总数，不进清单。
            Err(_) => {
                skips.internal();
                continue;
            }
        };

        let path = match to_posix(relative) {
            Some(path) => path,
            None => {
                // 非 UTF-8 文件名：跳过，绝不 panic，也绝不塞替换字符。
                //
                // 这一档**拿不到真路径**，只能 lossy + 标记。用户照着
                // `U+FFFD` 去改名会找不到文件，所以 `path_lossy` 必须为真。
                let (path, path_lossy) = posix_lossy(relative);
                skips.record(ScanSkipEntry {
                    path: Some(path),
                    path_lossy,
                    reason: ScanSkipReason::NonUtf8Name,
                });
                continue;
            }
        };

        let file_type = entry.file_type();
        let is_dir = file_type.is_dir();

        if !is_dir && !file_type.is_file() {
            // 管道 / socket / 设备文件 / 软链，不是笔记。
            //
            // 软链也落在这一支：`follow_links(false)` 下它是一个独立目录项，
            // 既不是 `is_dir()` 也不是 `is_file()`（B-12 的可见边界）。
            skips.record(ScanSkipEntry {
                path: Some(path),
                path_lossy: false,
                reason: ScanSkipReason::NotARegularFile,
            });
            continue;
        }

        // 目录必须全收：否则中间层级断裂，树就接不起来。
        if !is_dir && !is_note(entry.path()) {
            continue;
        }

        let name = match entry.file_name().to_str() {
            Some(name) => name.to_owned(),
            // 不可达：`to_posix` 刚刚证明整条相对路径是 UTF-8，文件名是它的
            // 最后一段。留着是为了让**五处**跳过各有明确归属，而不是为了它
            // 真的会发生——真发生了就是内部不变量破了，用户修不了。
            None => {
                skips.internal();
                continue;
            }
        };

        // 上限数的是**条目**（目录 + 文件），不是笔记。`>=` 而不是 `>`：
        // 收满 `max_entries` 条就停，`limit_entries` 是「最多愿意收多少条」。
        if flat.len() >= opts.max_entries {
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
        limit_entries: opts.max_entries,
        truncated,
        skipped: skips.total,
        skipped_detail: skips.entries,
        skipped_truncated: skips.truncated,
        ignore_rules: rules,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// 读取单篇笔记。
///
/// 路径必须经由 [`ResolvedVault`]，因此天然拒绝目录穿越。单篇调用用它就够了；
/// **批量读取请用 [`read_note_in`]**——本函数每次都会重新规范化库根、
/// 重新逐级查重解析点，一万篇就是两万次 `canonicalize`。
pub fn read_note(root: &Path, rel: &str) -> Result<NoteContent, KiError> {
    read_note_in(&ResolvedVault::open(root)?, rel)
}

/// 用已经打开好的库句柄读一篇。**建索引的热路径走这条。**
///
/// 与 [`read_note`] 的语义逐字相同，差别只在路径闸的开销：
/// 库根只规范化一次、父目录链按去重后的目录集合各查一次（见 [`ResolvedVault`]）。
///
/// 句柄的生命周期决定了缓存的新鲜度。调用方必须让它随「一次批处理」结束，
/// 不许存进任何活得更久的结构——理由写在 [`ResolvedVault`] 的「缓存只能是
/// 单批次的」一节。
pub fn read_note_in(vault: &ResolvedVault, rel: &str) -> Result<NoteContent, KiError> {
    let absolute = vault.resolve(rel)?;

    // 一次 `open` 拿到 handle，之后所有元数据都从 handle 取（`fstat`）。
    //
    // 为什么不是 `fs::metadata` + `fs::read`：那是**两次** `CreateFileW`
    // （`fs::read` 内部自己 open 一次）。本机实测 path stat 147µs、open 619µs，
    // 多这一次 stat 就是每篇多 14% 的固定成本，10k 篇白花 1.5 秒。
    // 而 `fstat`（`file.metadata()`）实测只加 6µs——它不碰路径。
    let mut file = match std::fs::File::open(&absolute) {
        Ok(file) => file,
        // Windows 上 `CreateFileW` 不带 `FILE_FLAG_BACKUP_SEMANTICS` 打不开目录，
        // 于是「目标是目录」会以权限错误的形式冒出来。补一次判定把它归回
        // `NotAFile`——这条只在失败路径上，不进热路径。
        // （Linux 上打开目录是成功的，由下面的 `is_file()` 拦。）
        Err(err) => {
            return Err(match std::fs::metadata(&absolute) {
                Ok(meta) if meta.is_dir() => KiError::NotAFile {
                    path: rel.to_owned(),
                },
                _ => KiError::io(rel, &err),
            })
        }
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

    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|e| KiError::io(rel, &e))?;

    // 编码探测与解码都在 [`crate::encoding`] 里：BOM → NUL 模式 → 严格
    // UTF-8 → 猜。**不要把 `String::from_utf8` 搬回这里**——无 BOM 的
    // UTF-16LE 纯 ASCII 是合法 UTF-8，在探测之前做严格 UTF-8 会把它静默
    // 读成一串带 NUL 的乱码。
    //
    // BOM 也在那里剥（UTF-8 的、UTF-16 的、UTF-32 的）：Windows 记事本
    // 保存 UTF-8 时会写 BOM，不去掉的话第一个 `# 标题` 前面多了个不可见字符，
    // 大纲解析与"首个标题"逻辑会同时失效。
    let decoded = encoding::decode(bytes).map_err(|_| KiError::InvalidEncoding {
        path: rel.to_owned(),
    })?;

    let (size, mtime) = stat(Some(meta));
    Ok(NoteContent {
        path: rel.to_owned(),
        decoded_from: decoded.transcoded_from().map(str::to_owned),
        content: decoded.text,
        size,
        mtime,
    })
}

/// 树里的一篇笔记（扁平化的结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// Vault 相对路径（POSIX）。
    pub path: String,
    /// 扫描时读到的字节数。
    pub size: u64,
    /// 扫描时读到的 mtime（Unix 毫秒）。
    pub mtime: i64,
}

/// 把树压成文件列表。
///
/// **顺序未定义**（内部用显式栈，与树的深度无关），调用方要确定性顺序就自己排。
///
/// 扫描与增量同步共用这一个函数：两边对「什么算一篇笔记」的判断只能有一份，
/// 否则会出现「树里有这篇、索引里没有」这种不报错的不一致——
/// 它表现为"侧栏看得到、搜不到"，是最难排查的一类 bug。
pub fn flatten_files(tree: &FileNode) -> Vec<FileRef> {
    let mut out = Vec::new();
    let mut stack = vec![tree];
    while let Some(node) = stack.pop() {
        match &node.children {
            Some(children) => stack.extend(children.iter()),
            None => out.push(FileRef {
                path: node.path.clone(),
                size: node.size,
                mtime: node.mtime,
            }),
        }
    }
    out
}

/// 是否应该继续下降。
///
/// `depth() == 0` 永远放行——用户完全可能选了一个名叫 `dist` 或 `target`
/// 的目录作为 Vault，此时把根剪掉会导致"选了文件夹却什么都没有"。
///
/// # 只看名字，不看路径
///
/// 这是刻意的，也是「误伤」的精确机制：`my-notes/dist/` 与根下的 `dist/`
/// 一视同仁地被剪掉。改成看路径要给 `filter_entry` 传整条路径并做前缀匹配，
/// 代价高一个数量级，而且会让「为什么这个目录能进」不可解释——用户看到
/// 一个能进的 `dist` 和一个不能进的 `dist`，没有任何规则能解释这个差别。
/// 配置里的路径会被**明确拒绝**并给出理由（`ignore::check_names`）。
///
/// 匹配表在扫描开头编译一次（[`ignore::Matcher`]），这里只做名字比较。
fn should_descend(entry: &DirEntry, matcher: &ignore::Matcher) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if entry.file_type().is_dir() {
        !matcher.ignores_dir(name.as_ref())
    } else {
        !matcher.ignores_file(name.as_ref())
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
pub(crate) fn stat(meta: Option<std::fs::Metadata>) -> (u64, i64) {
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

    fn entry(path: &str, reason: ScanSkipReason) -> ScanSkipEntry {
        ScanSkipEntry {
            path: Some(path.to_owned()),
            path_lossy: false,
            reason,
        }
    }

    /// 清单上限是**清单**的上限，不是总数的上限。
    ///
    /// 60 条非 UTF-8 文件名：`skipped` 必须是 60（精确），清单只能是 50，
    /// `truncated` 必须为真。UI 显示的是 60——拿清单长度当总数会让用户
    /// 以为「只有 50 个问题」，而剩下 10 个永远查不到。
    #[test]
    fn skip_log_caps_the_list_but_keeps_the_total_exact() {
        let mut log = SkipLog::default();
        for index in 0..60 {
            log.record(entry(&format!("gbk-{index:02}.md"), ScanSkipReason::NonUtf8Name));
        }

        assert_eq!(log.total, 60, "总数必须精确，含清单外的 10 条");
        assert_eq!(log.entries.len(), SKIP_LIST_MAX, "清单只保留前 50 条");
        assert!(log.truncated, "清单被截断时必须为真");
        assert_eq!(log.entries[0].path.as_deref(), Some("gbk-00.md"));
        assert_eq!(log.entries[SKIP_LIST_MAX - 1].path.as_deref(), Some("gbk-49.md"));
    }

    /// `Internal` 只进总数，不进清单（红线：用户修不了的别列给他）。
    ///
    /// 关键在最后两条断言：`total` 比清单长，但 `truncated` 必须是**假**。
    /// 拿 `total > entries.len()` 推 `truncated` 会在这里红——而那种写法
    /// 会让「有 1 条 Internal」的库在 UI 上说「只列出前 1 条」，纯属编造。
    #[test]
    fn skip_log_keeps_internal_out_of_the_list() {
        let mut log = SkipLog::default();
        log.internal();
        log.record(entry("locked", ScanSkipReason::Permission));

        assert_eq!(log.total, 2, "Internal 也算一条");
        assert_eq!(log.entries.len(), 1, "但清单里只有能定位的那一条");
        assert!(!log.truncated, "清单没被截断，只是有一条不该进");
        assert_eq!(log.entries[0].reason, ScanSkipReason::Permission);
    }

    /// 60 条 `Internal`：总数 60、清单 0 条、`truncated` 仍然是假。
    #[test]
    fn skip_log_never_truncates_on_internal_entries() {
        let mut log = SkipLog::default();
        for _ in 0..60 {
            log.internal();
        }

        assert_eq!(log.total, 60);
        assert!(log.entries.is_empty());
        assert!(!log.truncated);
    }

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
