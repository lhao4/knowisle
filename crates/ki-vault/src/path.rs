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
//!
//! [`ResolvedVault`] 把这条代价从「每条路径」降到「每个父目录」：同一批读取里
//! 父目录链只查一次，库根只规范化一次。句柄的生命周期必须是一次批处理，
//! 理由见它自己的文档。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use ki_core::KiError;

/// 本应用的元数据目录（Vault 相对路径的第一级）。
///
/// 它同时出现在 `scan::IGNORED_DIRS` 里——两处引用同一个常量，是为了让
/// "扫描器看不见它" 与 "破坏性操作不许碰它" 不会各自漂移。
///
/// 定义搬到了 `ki-core`：索引库（`ki-store`）也要用同一个名字，而它不该为了
/// 一个字符串常量反向依赖文件系统层。这里只做再导出，`crate::path::METADATA_DIR`
/// 的写法在 `ki-vault` 内部保持不变。
pub(crate) use ki_core::METADATA_DIR;

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
/// 回收站清单（`crate::trash`）在读记录时也用它做**显示前**的过滤：
/// 清单是磁盘上的普通文件，里面的 `origin` 可能被改坏。
///
/// 拒绝的东西：
/// - `NUL` 字节（会让底层 syscall 直接报错，提前拒掉好给出明确错误）
/// - 绝对路径与带根路径（含 Windows 盘符、UNC、`\\?\` verbatim 前缀）
/// - 任何非 `Normal` / `CurDir` 的组件，其中 `ParentDir` 就是防目录穿越的第一道闸
pub(crate) fn validate_rel(rel: &str) -> Result<&Path, KiError> {
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

/// 一个已经规范化过的 Vault 根，外加**单批次内**的父目录缓存。
///
/// # 它解决什么
///
/// 逐篇 `read_note` 时，[`resolve_in_vault`] 每篇都要做三件事：
/// ① `dunce::canonicalize(root)`；② 逐组件 `symlink_metadata`；
/// ③ `dunce::canonicalize(目标)`。10k 篇 = 2 万次 canonicalize + 3 万次 stat，
/// 实测占掉建库时间的绝大部分（ADR-007「10k 库的成本结构」）。
///
/// 句柄把 ① 提到构造时做一次，把 ② 按**去重后的父目录集合**做一次，
/// ③ 只在父目录上做。热路径于是变成「一次 open + N 次 read」。
///
/// # 缓存只能是单批次的
///
/// **绝不许把这个句柄放进进程级缓存**（`ki-ipc` 的 `search::cache`），
/// 也不许挂在任何活过一轮调用的结构上。缓存跨过「一次同步」之后，
/// 它藏起来的东西就变成：父目录在这期间变成了软链，而第一万篇仍然读到
/// 软链变成之前的世界——用户看到的是「搜索找到了一个库外的文件」。
/// 那正是 B-09 的坑（外部改动不可见），只不过这次是我们自己制造的。
///
/// 句柄用完即弃：随 `sync()` 的栈帧一起消失，下一次同步从零开始。
/// 字段用 `RefCell` 而不是 `Mutex`，就是为了让「共享到别的线程 / 放进静态」
/// 在编译期就不成立，而不是靠注释提醒。
///
/// # 两道闸都还在
///
/// - 第一道（[`reject_reparse_points`]）按父目录链跑一次；**最终组件**
///   每次都单独查一次 `symlink_metadata`——它才是「这个名字本身」。
/// - 第二道拆成两半：父目录的 `canonical` 必须落在库内（插入缓存时验一次），
///   拼上文件名之后再复核一次 `starts_with`。
///
/// 为什么可以不再把目标整个 `canonicalize` 一遍，见 [`Self::resolve`]。
pub struct ResolvedVault {
    /// 已规范化（`dunce`，不是 `std::fs`）的库根。
    ///
    /// 换成 `std::fs::canonicalize` 会得到 `\\?\E:\vault`，而扫描器产出的是
    /// `E:\vault`——`starts_with` 静默为 false，症状不是报错，是「所有笔记都
    /// PathEscape」。这个坑在模块文档第 2 条里写着，别再踩。
    root: PathBuf,
    /// 规范化后的**父目录相对路径** → 规范化后的绝对父目录。
    ///
    /// 键必须是规范化后的相对路径，不能是调用方传来的原始字符串：
    /// `a/./b` 与 `a/b` 是同一个目录，用原始串做键会让它们各占一格，
    /// 缓存对 `./notes/a.md` 这种常见写法直接失效。
    parents: RefCell<HashMap<PathBuf, PathBuf>>,
}

impl ResolvedVault {
    /// 规范化库根。这一步是整个句柄唯一一次「碰库根」。
    pub fn open(root: &Path) -> Result<Self, KiError> {
        let root = dunce::canonicalize(root)
            .map_err(|err| KiError::io(root.to_string_lossy().into_owned(), &err))?;
        Ok(Self {
            root,
            parents: RefCell::new(HashMap::new()),
        })
    }

    /// 已规范化的库根。调用方拿它做 `strip_prefix` 之类的纯路径运算。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 解析一条 Vault 相对路径。**这是热路径的入口。**
    ///
    /// 与 [`resolve_in_vault`] 的差别只有一条：返回值最后一段的**大小写**
    /// 是调用方传的，不保证与磁盘一致。这在三个平台上都不改变可读性：
    /// Windows / macOS 的文件系统不区分大小写，Linux 上大小写本来就敏感、
    /// 两者在「传错大小写」时都报 `NotFound`。需要「磁盘真名」的调用方
    /// （删除与重命名要把原名写进回收站清单）用 [`Self::resolve_real`]。
    ///
    /// # 为什么可以省掉目标那一次 `canonicalize`
    ///
    /// 旧的第二道闸是 `canonicalize(root.join(rel)).starts_with(canonical_root)`。
    /// 它的实质是「穿透所有链接之后，真实落点还在库内」。而在**第一道闸已经
    /// 确认路径上没有重解析点**的前提下，`canonicalize(parent.join(name))`
    /// 恒等于 `canonicalize(parent).join(name)`：父目录那一段已经规范化过，
    /// 最后一段是普通文件名（不含分隔符、不是 `.`/`..`、不是链接）。
    /// 所以「父目录 canonical + 文件名」与「目标整体 canonical」是同一个结果，
    /// 省掉的只是重复穿透父目录前缀的那部分工作。
    ///
    /// 这个等价关系依赖第一道闸，**不是**独立成立的：如果哪天有人把
    /// `reject_reparse_points` 删了或放宽了，这里的 `starts_with` 就只剩
    /// 「父目录在库内」这一半，最终组件指向库外的软链会被放行。
    ///
    /// 已知缺口（如实记录，见 ADR-007）：第一道闸看不见的重解析点
    /// （Windows 的卷挂载点、Unix 的 bind mount）在新实现下不再被第二道闸
    /// 兜住。旧实现靠 `canonicalize` 后的前缀变化拦住它们。判定这是可接受的：
    /// 挂载点必须落在目录上，而 `read_note` 随后会以 `NotAFile` 拒绝目录；
    /// 其余读路径的调用方拿到的路径仍在库内，只是落点可能跨了卷。
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, KiError> {
        let rel_path = validate_rel(rel)?;

        let Some(file_name) = rel_path.file_name() else {
            // `""` 与 `"."` 都指向库根本身。旧实现走 `canonicalize(root.join(""))`
            // 也是这个结果，`resolve_real` 会再规范化一次，语义一致。
            return Ok(self.root.clone());
        };

        let parent_rel = rel_path.parent().unwrap_or_else(|| Path::new(""));
        let target = self.parent(parent_rel, rel)?.join(file_name);

        // 第二道闸的后半：拼上文件名之后再复核一次。
        //
        // 父目录已经验证过在库内、`file_name` 是单组件，所以这一句在
        // 当前实现下恒真。保留它不是为了"以后可能有用"，而是为了让
        // 「越界检查」这件事在读路径上有一个**唯一、显式**的落点——
        // 删掉它，第二道闸就只剩父目录那一半，而那一半在
        // `parent()` 里、离"返回路径"隔着一个函数调用。
        if !target.starts_with(&self.root) {
            return Err(KiError::PathEscape {
                path: rel.to_owned(),
            });
        }

        // 第一道闸的最后一跳。父目录缓存省掉的只是「父目录链」，
        // 最终组件每次都查：它才是用户点名要的那个东西，也是
        // `a.md -> b.md`（都在库内）这类"改得到但看不见"的唯一拦截点。
        //
        // 不存在不算错：读路径随后会以 `NotFound` 报出来，写路径允许新建。
        if let Ok(meta) = std::fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                return Err(KiError::PathEscape {
                    path: rel.to_owned(),
                });
            }
        }

        Ok(target)
    }

    /// 与 [`resolve_in_vault`] 逐字等价：多一次目标的 `canonicalize`，
    /// 换来「返回值最后一段是磁盘上的真名」。
    ///
    /// 只有真正需要真名的调用方该用它（`delete_note` 把原名写进回收站清单、
    /// `rename_note` 按真名落回）。读路径不需要：它随后只会 `open` 这个路径，
    /// 而三个平台的文件系统要么不敏感、要么本来就要求精确匹配。
    pub fn resolve_real(&self, rel: &str) -> Result<PathBuf, KiError> {
        let target = self.resolve(rel)?;
        let canonical = dunce::canonicalize(&target).map_err(|err| KiError::io(rel, &err))?;

        // 纵深防御：`resolve` 已经拒绝了最终组件的重解析点，所以这里
        // 规范化不会改变落点。留着是为了让本函数单独看也是完整的。
        if !canonical.starts_with(&self.root) {
            return Err(KiError::PathEscape {
                path: rel.to_owned(),
            });
        }

        Ok(canonical)
    }

    /// 父目录链的唯一实现，带缓存。
    ///
    /// 命中时**跳过** `reject_reparse_points` 与 `canonicalize`——这正是缓存
    /// 的全部收益，也正是「缓存只能是单批次的」这条约束的来由：在同一个句柄
    /// 的生命周期里，如果父目录从普通目录变成软链，这里看不见。
    fn parent(&self, parent_rel: &Path, rel: &str) -> Result<PathBuf, KiError> {
        // 只留 `Normal` 段：这一步得到的键对 `a/./b` 与 `a/b` 是同一个。
        //
        // **不能写成 `components().collect::<PathBuf>()`**：`collect` 是逐个
        // `push`，`CurDir` 的 `as_os_str()` 是 `.`，于是 `./a` 会被拼回 `./a`
        // ——键没规范化，缓存对 `./notes/a.md` 这种常见写法直接失效。
        // （`..` 已在 `validate_rel` 里被拒，不会到这里。）
        let key: PathBuf = parent_rel
            .components()
            .filter_map(|component| match component {
                Component::Normal(segment) => Some(segment),
                _ => None,
            })
            .collect();

        if let Some(hit) = self.parents.borrow().get(&key) {
            return Ok(hit.clone());
        }

        // 第一道闸：父目录链上不许有任何重解析点。必须在 `canonicalize`
        // 之前——它会把链接穿透成真实落点，穿透完就再也看不出"原本是链接"。
        reject_reparse_points(&self.root, &key, rel)?;

        // 父目录必须存在。写入不会顺手替用户造目录，读取更没有理由。
        let canonical =
            dunce::canonicalize(self.root.join(&key)).map_err(|err| KiError::io(rel, &err))?;

        // 第二道闸的前半：父目录的真实落点必须在库内。
        // 这一条拦住的是「中间层是逃逸链接」——第一道闸理论上已经拦了，
        // 但它是纵深防御，且它是**缓存值**的准入条件：验证过的值才许进缓存。
        if !canonical.starts_with(&self.root) {
            return Err(KiError::PathEscape {
                path: rel.to_owned(),
            });
        }

        self.parents.borrow_mut().insert(key, canonical.clone());
        Ok(canonical)
    }
}

/// 把前端传来的 Vault 相对路径解析为绝对路径，并保证它没有逃出 Vault。
///
/// 这是所有"按路径**读**操作"的**逐次**入口。绕过它直接 `root.join(user_input)`
/// 等于把任意文件读取漏洞交给前端。
///
/// # 批量读取请用 [`ResolvedVault`]
///
/// 本函数每次调用都重新规范化库根、重新逐级查重解析点——对单篇操作无所谓，
/// 对「一万篇」就是 2 万次 canonicalize。句柄版的语义与它逐字相同
/// （内部就是 `ResolvedVault::open(root)?.resolve_real(rel)`），
/// 只是把这两件事提到批次外面做。
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
/// 句柄版把这个窗口从「一次检查」放大到「一个批次」：父目录在批次中途变成
/// 软链时，后续几篇看不见。这是缓存换来的速度的代价，边界是句柄的生命周期。
///
/// # 写入请用 [`resolve_for_write`]
///
/// 本函数要求目标**已存在**（`canonicalize` 的前提），因此无法用于新建笔记。
/// 写入路径的额外约束见 [`resolve_for_write`]。
pub fn resolve_in_vault(root: &Path, rel: &str) -> Result<PathBuf, KiError> {
    ResolvedVault::open(root)?.resolve_real(rel)
}

/// 只回答「这条路径的**父目录链**可用吗」，不回答「这个名字能不能用」。
///
/// # 为什么必须和 `resolve_for_write` 分开
///
/// 这两件事被同一个函数干了很久，后果是**判据不可分**：调用方拿到一个
/// `Err` 时无法知道是"目录没了"还是"名字被一个同名目录占了"。回收站恢复
/// 需要区分它们——前者该降级到库根，后者该在原目录里改名成 `c 2.md`。
///
/// 不拆开就只能去 match 错误文案，而那等于把判据藏在字符串里：改一句
/// 提示语就会静默改变行为，且没有任何测试会红。
///
/// 所以这里的边界是**最后一段之前**：
///
/// - 做：语法校验、父目录链逐级拒绝重解析点、父目录必须存在、
///   规范化后仍在 Vault 内；
/// - 不做：最终组件的类型（是目录？是软链？已经有人占了？）——那是
///   [`crate::trash`] 的 `occupied` 与 [`resolve_for_write`] 各自的事。
///
/// 返回 `KiError::Io { kind: NotFound }` 表示**父目录不存在**（这是恢复
/// 唯一会降级的情形）；其余错误都表示"用户需要去处理"，不是"我们替他换个地方"。
pub(crate) fn ensure_parent_dir_usable(root: &Path, rel: &str) -> Result<(), KiError> {
    resolve_parent(root, rel).map(|_| ())
}

/// 父目录链的唯一实现：校验 → 逐级查重解析点 → 规范化父目录 → 越界检查。
///
/// 返回**规范化后的父目录**。`ensure_parent_dir_usable` 与 [`resolve_for_write`]
/// 都走它，所以"什么算父目录可用"只有一份定义。
fn resolve_parent(root: &Path, rel: &str) -> Result<PathBuf, KiError> {
    let rel_path = validate_rel(rel)?;

    let canonical_root = dunce::canonicalize(root)
        .map_err(|e| KiError::io(root.to_string_lossy().into_owned(), &e))?;

    // 只查父目录链：最终组件若是软链，写入会被它带到一个别的地方——那件事
    // 由 `resolve_for_write` 单独判（见那里的注释），不归"父目录可用"管。
    let parent_rel = rel_path.parent().unwrap_or_else(|| Path::new(""));
    reject_reparse_points(&canonical_root, parent_rel, rel)?;

    // 父目录必须已存在：写入不会顺手替用户造目录，那属于上层的产品决策。
    let canonical_parent =
        dunce::canonicalize(canonical_root.join(parent_rel)).map_err(|e| KiError::io(rel, &e))?;

    if !canonical_parent.starts_with(&canonical_root) {
        return Err(KiError::PathEscape {
            path: rel.to_owned(),
        });
    }

    Ok(canonical_parent)
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

    // 父目录链：与回收站恢复共用同一份判据（见 `ensure_parent_dir_usable`）。
    let canonical_parent = resolve_parent(root, rel)?;
    let target = canonical_parent.join(file_name);

    // 最后一段单独判。重解析点先于目录判：一个指向目录的软链用
    // `symlink_metadata` 看是"软链"而不是"目录"，两种错都该报 `PathEscape`
    // ——它的危害是"写到别处去了"，比"写到一个目录上"严重。
    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => Err(KiError::PathEscape {
            path: rel.to_owned(),
        }),
        Ok(meta) if meta.file_type().is_dir() => Err(KiError::NotAFile {
            path: rel.to_owned(),
        }),
        // 不存在（新建）或读不出来（让随后的写操作报它自己的错）。
        _ => Ok(target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 第二道闸必须**独立**成立。
    ///
    /// 这条用例造不出真实场景，所以直接污染缓存来模拟「第一道闸没拦住」
    /// 之后的世界：能骗过 `is_symlink()` 的重解析点（Windows 卷挂载点、
    /// Unix bind mount）都需要管理员权限，CI 与开发机都建不出来。
    /// 验的是第二道闸的代码路径本身，不是某个平台特性。
    ///
    /// 为什么它必须存在：ADR-007 用「第一道闸是唯一真防线」论证了
    /// 省掉目标那次 `canonicalize` 是安全的。那个论证的**代价**就是第二道闸
    /// 在日常路径上永不触发——于是它唯一的回归保护就是这条注入用例。
    #[test]
    fn resolve_rejects_a_cached_parent_outside_the_root() {
        let vault = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.md"), "SECRET").unwrap();

        let handle = ResolvedVault::open(vault.path()).unwrap();
        // 直接往缓存里塞一个库外父目录。真要被走到，只可能是第一道闸
        // 对某个重解析点失明。
        handle.parents.borrow_mut().insert(
            PathBuf::from("sub"),
            dunce::canonicalize(outside.path()).unwrap(),
        );

        let err = handle
            .resolve("sub/secret.md")
            .expect_err("缓存里的库外父目录必须被第二道闸拒绝");
        assert!(
            matches!(err, KiError::PathEscape { .. }),
            "应当是 PathEscape，实际 {err:?}"
        );
    }
}
