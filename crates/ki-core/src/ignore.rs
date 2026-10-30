//! 忽略规则：内置名单 + 用户配置的合并结果（ADR-014）。
//!
//! # 忽略是**第三种缺席**
//!
//! 树里少了一个目录，有三种完全不同的原因，用户的出口也各不相同：
//!
//! | 缺席 | 含义 | 用户看到 | 出口 |
//! |---|---|---|---|
//! | 跳过（ADR-013） | 「我试了，没成」 | 「跳过 N」+ 清单 | 改权限 / 改名 / 挪走 |
//! | 忽略（本模块） | 「我按规则没看」 | 本规则面板 | 改配置 + 重扫 |
//! | 不存在 | 磁盘上真没有 | 什么都没有 | 去建 |
//!
//! 所以忽略**不许**混进 `skipped`，也不许进跳过面板：那个数字回答的是
//! 「哪里出了问题」，而忽略是**用户自己下的命令**，不是问题。
//!
//! # 为什么不报「被忽略了多少篇」
//!
//! 剪枝意味着**根本没有下降**（`filter_entry`），所以「被剪掉的子树里有几篇
//! 笔记」这个数我们手上没有。想知道就得把整棵子树再走一遍——那等于把
//! `.git` / `node_modules` 重新遍历一次，而这正是当初剪掉它们的理由。
//! 代价见 `crates/ki-vault/tests/ignore_gate.rs` 的实测。
//!
//! # 内置名单是**猜出来的**
//!
//! 名单里两个方向都会误伤：该忽略的漏了（`_site`、`vendor`），或者用户真有
//! 一个叫 `dist`、`logseq` 的笔记目录被静默吞掉。后者无法靠加名字解决——
//! 加得越多，误伤面越大。所以名单必须可配置（B-05）。

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::METADATA_DIR;

/// 内置目录名单：命中即**不再下降**。
///
/// 名字分两类，忽略它们的理由并不相同：
///
/// - **其他工具的元数据目录**（`.git` / `.obsidian` / `.logseq` / `.trash` /
///   `.stversions`）：里面装的是别人工具的配置、备份和回收站，不是用户的笔记。
///   `.obsidian` 与 `logseq` 尤其关键——「打开现有的 Obsidian / Logseq 库」是
///   最主要的迁移入口，不忽略的话 `.obsidian/plugins/*/README.md`、
///   `logseq/bak/*.md` 会整片混进树里，用户第一眼看到的是一堆别人的文档和
///   重复笔记。
/// - **构建产物与依赖**（`node_modules` / `target` / `dist` / ...）：误把前端
///   仓库选成 Vault 时，这些目录能有几十万文件，剪掉它们是性能问题而不只是
///   观感问题。
///
/// `.knowisle` 单独说一句：这是我们自己的元数据目录（索引、缓存、历史、
/// 回收站，以及本刀新增的配置文件），它永远不该出现在笔记树里——这是
/// 「派生数据不属于用户内容」的边界。它与 [`METADATA_DIR`] 是同一个常量：
/// 扫描器「看不见它」与破坏性操作「不许碰它」必须指的是同一个名字。
///
/// # 名单为什么住在 `ki-core` 而不是 `ki-vault`
///
/// 三个消费者：扫描器（判）、配置校验（negate 只能从这份名单里减）、
/// 前端（通过 `ScanResult.ignoreRules` 原样显示）。前两个是 Rust，第三个
/// 拿的是序列化结果。放在领域层是因为它描述的是「什么不是笔记树」，
/// 与 `METADATA_DIR` 同一类东西；`ki-vault` 只负责**读配置**与**合并**。
///
/// 顺序有意义：UI 按这个顺序列出内置名单，改顺序会让用户以为规则变了。
pub const BUILTIN_DIRS: &[&str] = &[
    // 其他工具的元数据与备份
    ".git",
    ".obsidian",
    ".logseq",
    // 不带点的 `logseq` 也要忽略：Logseq 的图谱结构就是 `<graph>/logseq/bak/*.md`，
    // 里面是 pages 的全量历史副本。误伤面是一个真的叫 `logseq` 的笔记目录——
    // 概率低，且一旦有人抱怨就走配置化（negate），不要靠加名字解决。
    "logseq",
    ".trash",
    ".stversions",
    // 自己的元数据目录。与 `path::METADATA_DIR` 是同一个常量。
    METADATA_DIR,
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
///
/// 与目录名单分开是因为它们**命中后的动作不同**：目录是「不下降」，
/// 文件是「不 stat」。合成一张表就得在匹配时按 `is_dir` 分支，
/// 而那个分支没有任何一处需要。
pub const BUILTIN_FILES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

/// 永远不可 `negate` 的名字。
///
/// - `.git`：里面是版本库的 objects，10k 篇的库动辄几十万个文件。放它进树
///   等于把一次 30ms 的扫描变成一次几十秒的遍历。
/// - `.knowisle`：本应用自己的索引、回收站、备份与配置文件。放它进来会形成
///   一个闭环——扫描看见索引，索引又因为扫描而变化，而配置文件本身也会变成
///   「一篇笔记」。
///
/// 用户写了这两条不是「静默忽略」，而是**一条可读的配置错误**：他知道自己
/// 写的规则没生效，也知道为什么。
pub const UNNEGOTIABLE: &[&str] = &[".git", METADATA_DIR];

/// 用户要求停用的内置名字，能不能停用。
///
/// 扫描器与配置校验共用这一个判据——两处各写一份 `==` 迟早会漂。
pub fn is_negotiable(name: &str) -> bool {
    !UNNEGOTIABLE
        .iter()
        .any(|fixed| fixed.eq_ignore_ascii_case(name))
}

/// 这个名字在不在内置名单里（目录或文件，大小写不敏感）。
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_DIRS
        .iter()
        .chain(BUILTIN_FILES.iter())
        .any(|known| known.eq_ignore_ascii_case(name))
}

/// 本次扫描**生效**的忽略规则。
///
/// # 它是诊断数据，不是配置
///
/// 它跟着 [`crate::ScanResult`] 一起回来，描述的是「这一次扫描按什么规则看的」。
/// 不落盘、不进索引库：配置文件才是用户输入的唯一真相源，这份只是它在
/// 这一次扫描里的投影。
///
/// # `builtin` 是**原文**，`negate` 不从这里删
///
/// 删掉的话就没人能回答「哪一条被我停用了」——而那正是用户改完配置之后
/// 最想确认的一件事。生效集合 = `builtin - negate + extra`，由读者自己算，
/// 因为两个集合分开显示比一个合并后的列表更能解释「为什么」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IgnoreRules {
    /// 内置目录名单原文（[`BUILTIN_DIRS`]，含那些被 negate 掉的）。
    pub builtin: Vec<String>,
    /// 用户额外忽略的名字（`[ignore] extra`）。
    pub extra: Vec<String>,
    /// 用户停用的内置名字（`[ignore] negate`）。只含**真的**停用成功的。
    pub negate: Vec<String>,
    /// 这次扫描去读的配置文件路径（绝对）。`None` = 根本没去看
    /// （[`IgnoreRules::builtin_only`]，用于测试与「只看内置名单」的场景）。
    ///
    /// # 为什么路径要跟着规则一起回来
    ///
    /// 面板上必须写出「去改哪个文件」。让前端自己拼 `<root>/.knowisle/config.toml`
    /// 等于把 `METADATA_DIR` 复制出第四份——而这个名字已经有三处在用同一个常量了。
    /// 存在与否不影响这个字段：**没读过**才是 `None`。
    pub config_path: Option<String>,
    /// 配置读不出来 / 写坏时的说明。`None` = 配置不存在，或存在且合法。
    ///
    /// # 为什么错误跟规则一起回来，而不是让 `vault_scan` 报错
    ///
    /// 配置写坏**不许**导致打不开库（B-05 的硬约束）。所以它是一条**可见的
    /// 降级**：规则退回内置默认，同时把「为什么」带给 UI。报错的话用户会
    /// 卡在一个打不开的库前面，而他改不了配置——他甚至不知道配置文件在哪。
    pub config_error: Option<IgnoreConfigError>,
}

impl IgnoreRules {
    /// 一份配置都没有时的形状：只有内置名单。
    ///
    /// 它同时是「配置写坏」的兜底值——两者的**规则**完全相同，差别只在
    /// `config_error` 有没有值。这也是判据「配置文件不存在时行为与今天
    /// 逐字节相同」的实现方式：不存在 → 这个函数 → 与改动前同一条路径。
    pub fn builtin_only() -> Self {
        Self {
            builtin: BUILTIN_DIRS.iter().map(|name| (*name).to_owned()).collect(),
            extra: Vec::new(),
            negate: Vec::new(),
            config_path: None,
            config_error: None,
        }
    }

    /// 合并一份**已校验**的用户配置。
    ///
    /// 调用方必须先用 `ki_vault::ignore::load` 校验过；这里不再检查
    /// `.git` / `.knowisle`，因为「拒绝」必须带行号，而那是解析器的事。
    /// 扫描器仍会在建匹配表时再挡一次（纵深防御，见 `Matcher::new`）。
    pub fn with_user(extra: Vec<String>, negate: Vec<String>) -> Self {
        Self {
            builtin: BUILTIN_DIRS.iter().map(|name| (*name).to_owned()).collect(),
            extra,
            negate,
            config_path: None,
            config_error: None,
        }
    }

    /// 内置默认 + 一条配置错误。**规则与 [`Self::builtin_only`] 逐字相同。**
    pub fn with_error(error: IgnoreConfigError) -> Self {
        Self {
            config_error: Some(error),
            ..Self::builtin_only()
        }
    }

    /// 有没有用户配置（不含错误）。
    pub fn has_user_rules(&self) -> bool {
        !self.extra.is_empty() || !self.negate.is_empty()
    }
}

/// 配置文件写坏时给用户看的那一行。
///
/// # 三个字段缺一不可
///
/// - `message` 是**一句人话**：说清哪条规则不对、应该怎么写。
/// - `path` 是**哪个文件**：用户可能同时开着几个库，不说路径等于让他去猜。
/// - `line` 是**哪一行**：配置文件是手写的，没有行号就只能通读一遍。
///   拿不到时（比如整个文件读不出来）为 `None`——那时行号没有意义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IgnoreConfigError {
    /// 配置文件的绝对路径。
    pub path: String,
    /// 出错的行号（从 1 开始）。整个文件级失败时为 `None`。
    pub line: Option<u32>,
    /// 一句人话。
    pub message: String,
}
