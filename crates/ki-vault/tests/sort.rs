//! 显示序（ADR-016）的验收：树里同层条目的顺序。
//!
//! 每条用例都断言**具体顺序**，不是「不 panic」。判据来自施工单 §3：
//! 1. 固定语料 ≥10 组；2. 数字感知；3. 目录仍在前。
//!
//! 这里走的是 `scan_vault`（真文件、真目录），不是直接调 `sort::compare`：
//! 排序点只有一个（`build_tree`），绕过它测排序器等于没测接线。

use std::fs;
use std::path::Path;

use ki_vault::{scan_vault, ScanOptions};
use tempfile::TempDir;

fn write_file(root: &Path, relative: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("创建父目录失败");
    }
    fs::write(path, "# x\n").expect("写入测试文件失败");
}

/// 扫一遍，返回根层子节点的名字（保持树里的顺序）。
fn root_order(files: &[&str]) -> Vec<String> {
    let dir = TempDir::new().unwrap();
    for relative in files {
        write_file(dir.path(), relative);
    }
    let result = scan_vault(dir.path(), &ScanOptions::default()).expect("扫描不应失败");
    result
        .tree
        .children
        .unwrap_or_default()
        .iter()
        .map(|node| node.name.clone())
        .collect()
}

#[test]
fn pinyin_order_for_common_surnames() {
    // 码位序是 张(5F20) < 李(674E) < 王(738B)；拼音序是 li < wang < zhang。
    assert_eq!(
        root_order(&["张伟.md", "李娜.md", "王芳.md"]),
        vec!["李娜.md", "王芳.md", "张伟.md"]
    );
}

/// 多音字按**字**取常用读音，不按词分词（ADR-007 明确不做分词）。
///
/// ICU 的 `zh` 数据里带少量词级 contraction，所以「重庆」拿到的是 chong——
/// 这是 ICU 比裸拼音表多出来的一点，不是我们做的。上限写进 ADR-016：
/// 「重新」仍是 zhong，因为它的正确读音要分词才拿得到。
#[test]
fn polyphone_uses_the_character_reading() {
    // 重庆 = chong（早于 li）；重新 = zhong（晚于 zhi）。
    let order = root_order(&["重庆.md", "李娜.md", "知识管理.md", "重新.md"]);
    assert_eq!(
        order,
        vec!["重庆.md", "李娜.md", "知识管理.md", "重新.md"]
    );
}

#[test]
fn digits_are_compared_by_value() {
    assert_eq!(
        root_order(&["note10.md", "note2.md", "note100.md"]),
        vec!["note2.md", "note10.md", "note100.md"]
    );
}

#[test]
fn digits_inside_a_word_are_compared_by_value_too() {
    assert_eq!(
        root_order(&["a10b.md", "a2b.md"]),
        vec!["a2b.md", "a10b.md"]
    );
}

#[test]
fn digits_after_cjk_are_compared_by_value() {
    assert_eq!(
        root_order(&["第10章.md", "第2章.md"]),
        vec!["第2章.md", "第10章.md"]
    );
}

/// 大小写混排：大小写不参与主序，顺序由字母本身决定。
///
/// **不能用 `apple.md` / `Apple.md` 造这一组**：NTFS 与 APFS 默认大小写不敏感，
/// 第二个 `fs::write` 写的是同一个文件，用例会静默退化成「只有一篇」。
/// 「只差大小写」那一组在 `src/sort.rs` 的单元测试里（那里没有文件系统）。
#[test]
fn mixed_case_names_sort_by_their_letters() {
    assert_eq!(
        root_order(&["README.md", "api-guide.md", "API-Design.md"]),
        vec!["API-Design.md", "api-guide.md", "README.md"]
    );
}

/// 汉字整块排在拉丁字母前面。
///
/// 这不是「按读音交错」——ICU 的 `zh` 排序规则把汉字重排成一整块。
/// 要交错就得自己做拼音串比较（= 门禁① 否决的 `pinyin` 方案），
/// 代价写在 ADR-016 的失败模式表里。
#[test]
fn cjk_comes_before_latin_as_one_block() {
    assert_eq!(
        root_order(&["笔记.md", "apple.md", "阿里.md"]),
        vec!["阿里.md", "笔记.md", "apple.md"]
    );
}

#[test]
fn emoji_sorts_before_letters() {
    // ICU 把符号 / emoji 排在最前（与旧的码位序不同：😀 U+1F600 在旧实现里排最后）。
    let order = root_order(&["zzz.md", "😀emoji.md", "aaa.md"]);
    assert_eq!(order, vec!["😀emoji.md", "aaa.md", "zzz.md"]);
}

/// NFC 与 NFD 两种写法的 `é`：排序键相等，顺序由原名兜底（NFD 的 `e` 在前）。
///
/// 今天按码位比较时两者是「两个不同的字」；改完 ICU 认为它们**相等**——
/// 这是顺带修好的隐患，ADR-016 认领。
#[test]
fn nfc_and_nfd_are_the_same_letter() {
    let nfc = "caf\u{e9}.md";
    let nfd = "cafe\u{301}.md";
    let order = root_order(&[nfc, nfd]);
    assert_eq!(order, vec![nfd.to_owned(), nfc.to_owned()]);
}

#[test]
fn dirs_come_before_files() {
    assert_eq!(
        root_order(&["b.md", "A.md", "zzz/c.md"]),
        vec!["zzz", "A.md", "b.md"]
    );
}

/// 目录名排在同层文件名之后，仍然要排在文件前面——这条是判据 3 的后半句。
#[test]
fn a_dir_sorts_before_every_file_even_when_its_name_is_last() {
    assert_eq!(
        root_order(&["aaa.md", "mmm.md", "zzz/inner.md"]),
        vec!["zzz", "aaa.md", "mmm.md"]
    );
}

/// 同名（去掉扩展名后）的目录与文件：目录在前。
#[test]
fn a_dir_and_a_file_with_the_same_stem_keep_the_dir_first() {
    assert_eq!(
        root_order(&["note.md", "note/inner.md"]),
        vec!["note", "note.md"]
    );
}

/// 判据 4 的后半句：同一份夹具扫 3 次，树序**逐字节**相同。
///
/// 排序键来自 ICU 的 compiled_data（锁在 `Cargo.lock`），不是系统 ICU、
/// 不是环境变量、不是 locale 设置——所以这条能过。它是「前端不许用
/// `localeCompare`」的论据：同一份库在两台机器上必须给出同一串顺序。
#[test]
fn three_scans_produce_byte_identical_order() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for name in [
        "张伟.md",
        "李娜.md",
        "王芳.md",
        "重庆.md",
        "重新.md",
        "note2.md",
        "note10.md",
        "note100.md",
        "Apple.md",
        "笔记.md",
        "caf\u{e9}.md",
        "cafe\u{301}.md",
    ] {
        write_file(root, name);
    }
    write_file(root, "子目录/内部.md");

    let render = |tree: &ki_core::FileNode| {
        let mut out = String::new();
        fn walk(node: &ki_core::FileNode, out: &mut String, depth: usize) {
            for child in node.children.as_deref().unwrap_or(&[]) {
                out.push_str(&"  ".repeat(depth));
                out.push_str(&child.name);
                out.push('\n');
                if child.kind.is_dir() {
                    walk(child, out, depth + 1);
                }
            }
        }
        walk(tree, &mut out, 0);
        out
    };

    let first = render(&scan_vault(root, &ScanOptions::default()).unwrap().tree);
    for round in 0..2 {
        let again = render(&scan_vault(root, &ScanOptions::default()).unwrap().tree);
        assert_eq!(again, first, "第 {} 次重扫的树序与第一次不同", round + 2);
    }
}
