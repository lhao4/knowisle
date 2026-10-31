//! 显示序：侧栏树里同一层条目的先后。
//!
//! # 两套顺序，不要混
//!
//! - **走查序**（`WalkDir::sort_by_file_name()`，`scan.rs`）：按文件名的码位。
//!   它决定扫描上限先收下谁，也就是 ADR-015 的**截断边界**。本模块不参与。
//! - **显示序**（本模块）：拼音 + 数字感知。它决定用户在侧栏看到的顺序，
//!   也是快速打开空查询的顺序——前端不再自己排一遍（ADR-016 决策四）。
//!
//! 两者今天在纯 ASCII 的小库上看着一样，那是巧合，不是契约。
//!
//! # 为什么键要整表预计算
//!
//! `build_tree` 对每个 parent 分组各排一次，10k 条目约 1.3×10⁵ 次比较。
//! 每次比较都让 ICU 从头生成排序元素，就是 10⁵ 次 ICU 调用；预计算成字节串
//! 之后排序器只被调用 n 次，比较退化成 `Vec<u8>` 的 `cmp`（memcmp）。
//! 门禁③的实测见 ADR-016。
//!
//! # 为什么是 ICU4X（compiled_data）而不是手写拼音表
//!
//! 三个候选的数字与结论见 ADR-016 门禁①。一句话：手写表要自己做
//! 「多音字取常用读音 + 规范化 + 数字分段 + 大小写折叠」四件事，其中后两件
//! 是 Unicode 算法，不是一张表能解决的。`compiled_data` 把 CLDR 数据编进
//! 二进制，**不读系统 ICU**——同一份 `Cargo.lock` 在任何机器上给出同一串字节，
//! 这是「前端不许用 `localeCompare`」那条红线的 Rust 侧等价物。
//!
//! # 键只在一次扫描内可比
//!
//! 排序键的字节依赖 ICU 的数据版本（锁在 `Cargo.lock`）。跨版本、跨机器的键
//! **不保证**可比，所以它只在内存里活一次扫描：不落盘、不进 IPC、不进
//! [`ki_core::FileNode`]（红线：核心 serde 结构不加排序字段）。

use std::cmp::Ordering;

use icu_collator::options::{CollatorOptions, Strength};
use icu_collator::preferences::CollationNumericOrdering;
use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences};
use icu_locale_core::Locale;
use smallvec::SmallVec;

/// 排序用的语言。只影响汉字：ICU 的 `zh` 默认排序规则就是拼音序。
///
/// 不能省成 `Default`（root）：root 对汉字按码位，改完等于没改——
/// 第一版探针就踩了这个，输出与旧实现逐字相同。
const LOCALE: &str = "zh";

/// 排序器。构造一次，整表预计算键。
///
/// 不许在 `compare` 里构造它：`build_tree` 的每个 parent 分组都会排一次，
/// 那是 O(分组数) 次构造。
pub struct Sorter {
    collator: CollatorBorrowed<'static>,
}

impl Sorter {
    /// 构造失败只可能是「二进制里没编进 `zh` 排序数据」——那是构建配置错误，
    /// 不是运行期状况，所以直接炸而不是降级成码位序：降级会让排序悄悄退回
    /// 旧行为，而没有任何地方会报错。
    ///
    /// `sort::tests::the_sorter_builds` 会在 CI 里先跑一遍，把这种构建错误
    /// 挡在测试阶段，而不是用户点开库的那一刻。
    pub fn new() -> Self {
        let locale: Locale = LOCALE.parse().expect("`zh` 是合法 BCP-47");
        let mut prefs = CollatorPreferences::default();
        prefs.locale_preferences = (&locale).into();
        // 数字感知：`note2 < note10`。ICU 的默认值是 False。
        prefs.numeric_ordering = Some(CollationNumericOrdering::True);

        let mut options = CollatorOptions::default();
        // 二级 = 忽略大小写、仍区分重音。ICU 默认三级（区分大小写），
        // 那会把 `A.md` / `a.md` 的既有顺序改掉：大小写从「兜底」升成主序差异。
        options.strength = Some(Strength::Secondary);

        let collator = Collator::try_new(prefs, options)
            .expect("compiled_data 必须自带 zh 排序数据：缺了是构建配置错误");
        Self { collator }
    }

    /// 一个名字的排序键。字节序即排序序。
    pub fn key(&self, name: &str) -> SortKey {
        // 24 字节内联：10k 夹具实测平均键长 9.8 字节，几乎全部落在栈上。
        // 这不只是省分配——比较时也不用追一次指针。
        let mut bytes: SmallVec<[u8; 24]> = SmallVec::new();
        self.collator
            .write_sort_key_to(name, &mut bytes)
            .expect("SmallVec<[u8; N]> 的 CollationKeySink 是 Infallible");
        SortKey { bytes }
    }
}

impl Default for Sorter {
    fn default() -> Self {
        Self::new()
    }
}

/// 预计算好的排序键。
///
/// `Ord` 直接比字节：ICU 的排序键就是为「字典序 == 排序序」设计的。
/// 两个名字的键相等意味着**在这个强度下不可区分**（例如 `Apple` / `apple`，
/// 或 NFC / NFD 两种写法的 `é`），此时由 [`compare`] 用原名兜底。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SortKey {
    bytes: SmallVec<[u8; 24]>,
}

impl SortKey {
    /// 键的原始字节。给用例与 ADR 的实测表用，生产代码不该读它。
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// 参与比较的三样东西。
///
/// 单独成一个结构而不是三个参数：`compare` 的调用点只有一处，但用例里
/// 要手写十几次，三个裸参数（`bool, &str, &SortKey`）读起来全是位置记忆。
#[derive(Clone, Copy)]
pub struct Entry<'a> {
    /// 目录排在同层的文件前面。
    pub is_dir: bool,
    /// 原名。键相等时用它兜底，保证**全序**（同一次扫描里没有两个条目相等）。
    pub name: &'a str,
    /// 预计算的键。
    pub key: &'a SortKey,
}

/// 同层条目的显示序：目录在前 → 排序键 → 原名。
///
/// 第三级不是装饰：ICU 在二级强度下让 `Apple` 与 `apple` 的键**逐字节相同**，
/// 没有原名兜底，`sort_by` 的顺序就取决于排序算法的实现细节（不稳定排序时
/// 同一个库两次扫描可能给出不同顺序）。`a.md` / `A.md` 同理。
pub fn compare(a: Entry<'_>, b: Entry<'_>) -> Ordering {
    b.is_dir
        .cmp(&a.is_dir)
        .then_with(|| a.key.cmp(b.key))
        .then_with(|| a.name.cmp(b.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(names: &[&str]) -> Vec<String> {
        let sorter = Sorter::new();
        let mut keys: Vec<(&str, SortKey)> = names.iter().map(|n| (*n, sorter.key(n))).collect();
        keys.sort_by(|(na, ka), (nb, kb)| {
            compare(
                Entry { is_dir: false, name: na, key: ka },
                Entry { is_dir: false, name: nb, key: kb },
            )
        });
        keys.into_iter().map(|(n, _)| n.to_owned()).collect()
    }

    /// 构建配置的自检：compiled_data 缺了 `zh`，这一条先红。
    #[test]
    fn the_sorter_builds() {
        let sorter = Sorter::new();
        assert!(!sorter.key("a").as_bytes().is_empty());
    }

    /// 判据 1 的 é 一组：NFC 与 NFD 在排序键上**相等**，顺序由原名兜底。
    ///
    /// 今天按码位比较时两者不等（`é` U+00E9 vs `e` + U+0301），改完相等——
    /// 这是顺带修好的隐患，ADR-016 认领了它。
    #[test]
    fn nfc_and_nfd_have_equal_keys() {
        let sorter = Sorter::new();
        let nfc = sorter.key("caf\u{e9}");
        let nfd = sorter.key("cafe\u{301}");
        assert_eq!(nfc, nfd, "二级强度下重音写法不该改变排序键");
        // 键相等 → 原名兜底 → NFD（`e` 0x65）在前。
        assert_eq!(sorted(&["caf\u{e9}", "cafe\u{301}"]), vec!["cafe\u{301}", "caf\u{e9}"]);
    }

    /// 判据 1 的固定语料（20 个名字，ADR-016 的表就是它）。
    ///
    /// 放在单元测试而不是 `tests/sort.rs`：这里面有 `Apple` / `apple` 与
    /// NFC / NFD 两组**只差大小写或写法**的名字，NTFS 与 APFS 默认大小写不敏感，
    /// 落成文件会互相覆盖（用例静默退化成 19 个）。
    #[test]
    fn the_twenty_name_corpus() {
        let corpus = [
            "张伟", "李娜", "王芳", "重庆", "重新", "note2", "note10", "note100", "a2b", "a10b",
            "Apple", "apple", "笔记", "知识管理", "README", "caf\u{e9}", "cafe", "\u{1f600}emoji",
            "zzz", "阿里",
        ];
        assert_eq!(
            sorted(&corpus),
            vec![
                "\u{1f600}emoji",
                "阿里",
                "笔记",
                "重庆",
                "李娜",
                "王芳",
                "张伟",
                "知识管理",
                "重新",
                "a2b",
                "a10b",
                "Apple",
                "apple",
                "caf\u{e9}",
                "cafe",
                "note2",
                "note10",
                "note100",
                "README",
                "zzz",
            ]
        );
    }

    /// 判据 2：数字感知。
    #[test]
    fn digits_are_compared_by_value() {
        assert_eq!(
            sorted(&["note100", "note10", "note2"]),
            vec!["note2", "note10", "note100"]
        );
        assert_eq!(sorted(&["a10b", "a2b"]), vec!["a2b", "a10b"]);
    }

    /// 判据 4：大小写只差的两个名字有确定顺序，且两次结果相同。
    #[test]
    fn case_only_difference_is_total_and_stable() {
        let first = sorted(&["apple", "Apple"]);
        assert_eq!(first, vec!["Apple", "apple"]);
        for _ in 0..3 {
            assert_eq!(sorted(&["apple", "Apple"]), first);
        }
    }

    /// 判据 4：200 个构造名字的排序结果必须与两两比较一致（传递闭包）。
    ///
    /// 只验「排完是升序」不够——`sort_by` 用了一个不自洽的比较函数时，
    /// 结果可能既不是升序也不报错。这里对**每一对**都重新比一次。
    #[test]
    fn two_hundred_names_form_a_total_order() {
        let alphabet = [
            "a", "B", "z", "0", "2", "10", "阿", "张", "李", "王", "重", "é", "-", "_", "日", "Q",
        ];
        let mut names: Vec<String> = Vec::new();
        for (i, a) in alphabet.iter().enumerate() {
            for b in alphabet.iter() {
                names.push(format!("{a}{b}{i}"));
            }
        }
        names.truncate(200);
        assert_eq!(names.len(), 200);

        let sorter = Sorter::new();
        let mut keys: Vec<(&str, SortKey)> = names.iter().map(|n| (n.as_str(), sorter.key(n))).collect();
        keys.sort_by(|(na, ka), (nb, kb)| {
            compare(
                Entry { is_dir: false, name: na, key: ka },
                Entry { is_dir: false, name: nb, key: kb },
            )
        });

        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                let (na, ka) = &keys[i];
                let (nb, kb) = &keys[j];
                let ordering = compare(
                    Entry { is_dir: false, name: na, key: ka },
                    Entry { is_dir: false, name: nb, key: kb },
                );
                assert_eq!(
                    ordering,
                    Ordering::Less,
                    "`{na}` 排在 `{nb}` 前面，但两两比较给出 {ordering:?}"
                );
            }
        }

        // 再排一遍，逐项相同。
        let again = sorted(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let once: Vec<String> = keys.iter().map(|(n, _)| (*n).to_owned()).collect();
        assert_eq!(once, again, "同一份输入两次排序必须逐项相同");
    }

    /// 判据 3 的单元版：目录在前，哪怕它的名字排在后面。
    #[test]
    fn dirs_come_first_even_when_their_name_sorts_last() {
        let sorter = Sorter::new();
        let dir = sorter.key("zzz");
        let file = sorter.key("aaa");
        let ordering = compare(
            Entry { is_dir: false, name: "aaa", key: &file },
            Entry { is_dir: true, name: "zzz", key: &dir },
        );
        assert_eq!(ordering, Ordering::Greater, "文件不该排到目录前面");
    }
}
