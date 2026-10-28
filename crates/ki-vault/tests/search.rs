//! 全文索引的行为契约。
//!
//! 这些用例只碰纯逻辑（`SearchIndex`），不碰进程缓存、不碰 IPC。
//! 每条断言都对应一个具体的用户可见结论：大小写、中文、行号、偏移、上限、坏文件。

use std::fs;
use std::path::Path;

use ki_vault::{SearchIndex, MAX_HITS, MAX_HITS_PER_FILE, MAX_INDEX_BYTES};

/// 造一个临时 Vault，按 `(相对路径, 内容)` 写文件。
fn vault(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    for (path, content) in files {
        write(dir.path(), path, content);
    }
    dir
}

fn write(root: &Path, path: &str, content: &str) {
    let absolute = root.join(path);
    if let Some(parent) = absolute.parent() {
        fs::create_dir_all(parent).expect("创建父目录失败");
    }
    fs::write(absolute, content).expect("写文件失败");
}

fn search(root: &Path, query: &str) -> ki_core::SearchResult {
    let mut index = SearchIndex::build(root).expect("建索引失败");
    index.search(query, None)
}

/// 按 UTF-16 码元切片——模拟前端拿到的 `text.slice(from, to)`。
///
/// 不能直接写 `&text[from..to]`：那是**字节**下标，中文与 emoji 上会切歪。
fn slice_utf16(text: &str, from: u32, to: u32) -> String {
    let mut out = String::new();
    let mut unit = 0u32;
    for ch in text.chars() {
        let next = unit + ch.len_utf16() as u32;
        if unit >= from && next <= to {
            out.push(ch);
        }
        unit = next;
    }
    out
}

#[test]
fn ascii_case_is_folded_but_chinese_is_verbatim() {
    let dir = vault(&[("a.md", "Hello Foo\n第二行 知识库 子串\n")]);

    let lower = search(dir.path(), "foo");
    assert_eq!(lower.total, 1);
    assert_eq!(lower.hits[0].line, 1);
    assert_eq!(
        slice_utf16(&lower.hits[0].text, lower.hits[0].from, lower.hits[0].to),
        "Foo"
    );

    // 大写查询命中同一个小写位置——折叠是双向的，不是"查询必须小写"。
    let upper = search(dir.path(), "FOO");
    assert_eq!(upper.total, 1);
    assert_eq!(upper.hits[0].column, lower.hits[0].column);

    // 中文没有大小写，子串匹配天然可用。
    let chinese = search(dir.path(), "知识");
    assert_eq!(chinese.total, 1);
    assert_eq!(chinese.hits[0].line, 2);
}

#[test]
fn full_width_and_half_width_never_match_each_other() {
    let dir = vault(&[("a.md", "全角ＡＢＣ 与半角ABC\n")]);

    let half = search(dir.path(), "ABC");
    assert_eq!(half.total, 1, "半角查询必须只命中半角那一段");
    assert_eq!(
        half.hits[0].column, 9,
        "全角ＡＢＣ 占 3 个 UTF-16 码元，之后是「 与半角」4 个"
    );

    let full = search(dir.path(), "ＡＢＣ");
    assert_eq!(full.total, 1);
    assert_eq!(full.hits[0].column, 2);
}

#[test]
fn offsets_are_utf16_code_units() {
    // emoji 在 UTF-16 里是 2 个码元、在 UTF-8 里是 4 个字节。
    // 偏移算成字节或字符数都会在这里露馅。
    let dir = vault(&[("a.md", "🙂 foo bar\n")]);
    let result = search(dir.path(), "foo");
    assert_eq!(result.hits[0].column, 3);
    assert_eq!(result.hits[0].column_end, 6);
    assert_eq!(result.hits[0].from, 3);
    assert_eq!(result.hits[0].to, 6);
}

#[test]
fn crlf_lines_have_no_trailing_carriage_return() {
    let dir = vault(&[("a.md", "first\r\nfoo second\r\nlast\r\n")]);
    let result = search(dir.path(), "foo");
    assert_eq!(result.hits[0].line, 2);
    assert_eq!(result.hits[0].text, "foo second");
}

#[test]
fn one_line_yields_one_hit_even_if_it_repeats() {
    let dir = vault(&[("a.md", "hit hit hit\nhit again\n")]);
    let result = search(dir.path(), "hit");
    assert_eq!(result.total, 2, "同一行里的三次出现只算一条");
    assert_eq!(result.hits[0].line, 1);
    assert_eq!(result.hits[1].line, 2);
}

#[test]
fn per_file_cap_is_enforced() {
    let dir = vault(&[("a.md", "hit\nhit\nhit\nhit\nhit\n")]);
    let result = search(dir.path(), "hit");
    // `total` 数的是**行数**：写 5 行就是 5 行。`MAX_HITS_PER_FILE` 只限制
    // 返回几条——拿返回条数当行数的话，一篇写 10 次只算 3 次，而 `truncated`
    // 是 false（它只表示 total 是下界），UI 于是会理直气壮地少报。
    assert_eq!(result.total, 5, "total 是行数，不受每篇返回上限影响");
    assert_eq!(result.hits.len(), MAX_HITS_PER_FILE);
}

/// 多词查询是 **AND**：每个词都得出现在同一行里。
///
/// 旧实现把整串当一个 needle，`foo bar` 只能命中「foo bar」这个连续子串——
/// 于是「分开写的两个词」一条都搜不到。用户在搜索框里按空格分词是通用直觉，
/// 而旧语义把空格当成了正文的一部分。
#[test]
fn multi_word_query_is_an_and_of_substrings() {
    let dir = vault(&[
        ("both.md", "foo 在左，bar 在右\n"),
        ("only-foo.md", "只有 foo\n"),
        ("only-bar.md", "只有 bar\n"),
        ("adjacent.md", "foo bar\n"),
    ]);

    let result = search(dir.path(), "foo bar");
    let mut paths: Vec<&str> = result.hits.iter().map(|hit| hit.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["adjacent.md", "both.md"], "两个词都出现的行才算命中");
    assert_eq!(result.total, 2);

    // 词序无关：AND 是对称的。
    let swapped = search(dir.path(), "bar foo");
    assert_eq!(swapped.total, 2);
}

/// 多词命中的锚点取**最靠左**的那个词：行号/列号/摘要仍然指向一个真实位置。
#[test]
fn multi_word_hit_anchors_at_the_leftmost_word() {
    let dir = vault(&[("a.md", "bar 在前 foo 在后\n")]);
    let result = search(dir.path(), "foo bar");
    assert_eq!(result.hits.len(), 1);
    let hit = &result.hits[0];
    assert_eq!(hit.column, 0, "锚点是 bar 的起点，不是 foo 的");
    assert_eq!(
        slice_utf16(&hit.text, hit.from, hit.to),
        "bar",
        "from/to 指向锚点那个词"
    );
}

/// 拆词不能把「一行只出一条」的契约弄丢：同一行出现多次仍然只算一条。
#[test]
fn multi_word_query_still_yields_one_hit_per_line() {
    let dir = vault(&[("a.md", "foo bar foo bar\nfoo bar\n")]);
    let result = search(dir.path(), "bar foo");
    assert_eq!(result.total, 2, "两行各一条");
    assert_eq!(result.hits[0].line, 1);
    assert_eq!(result.hits[1].line, 2);
}

#[test]
fn global_cap_is_enforced_and_total_keeps_the_real_count() {
    let mut files = Vec::new();
    let bodies: Vec<String> = (0..80).map(|_| "hit\nhit\nhit\n".to_owned()).collect();
    let names: Vec<String> = (0..80).map(|i| format!("f{i:03}.md")).collect();
    for (name, body) in names.iter().zip(bodies.iter()) {
        files.push((name.as_str(), body.as_str()));
    }
    let dir = vault(&files);

    let result = search(dir.path(), "hit");
    assert_eq!(result.total, 240);
    assert_eq!(result.hits.len(), MAX_HITS);
    // 全扫路径读过每一篇，`total` 是准确值。`hits` 只返回 200 条是**另一件事**，
    // 它不代表数不清——`truncated` 只表示「total 是下界」。
    assert!(
        !result.truncated,
        "total 准确时不许标成下界，否则 UI 会把「共 240 条」说成「至少 240 条」"
    );
}

#[test]
fn file_name_hits_rank_before_content_hits() {
    let dir = vault(&[
        ("z/alpha.md", "正文里也有 alpha\n"),
        ("a/note.md", "alpha\n"),
    ]);
    let result = search(dir.path(), "alpha");
    assert_eq!(result.hits.len(), 2);
    assert_eq!(
        result.hits[0].path, "z/alpha.md",
        "文件名命中的那篇必须排前面"
    );
    assert!(result.hits[0].name_hit);
    assert!(!result.hits[1].name_hit);
}

#[test]
fn bad_files_are_skipped_without_failing_the_whole_index() {
    let dir = vault(&[("good.md", "可以搜到\n")]);

    // 认不出编码：`01 FF 02` 解出来只可能是控制字符，四道探测全过不去。
    // （M7 第八刀之前这里放的是 GBK 字节——它现在解得开了，不再是「坏文件」。）
    fs::write(dir.path().join("bad.md"), [0x01u8, 0xFF, 0x02]).expect("写坏文件失败");
    // 超过索引上限。内容全是 'x'，不含关键词，但必须计入 skipped。
    let huge = "x".repeat(MAX_INDEX_BYTES as usize + 1);
    fs::write(dir.path().join("huge.md"), huge).expect("写超大文件失败");

    let mut index = SearchIndex::build(dir.path()).expect("坏文件不该让建索引失败");
    let result = index.search("可以", None);
    assert_eq!(result.indexed_files, 1);
    assert_eq!(result.skipped_files, 2);
    assert_eq!(result.total, 1);
    assert_eq!(result.hits[0].path, "good.md");
}

/// GBK 笔记现在进得了索引，也搜得到——M7 第八刀的核心行为变更。
///
/// 改动前它是 `skipped_files`，表现为「侧栏看得见、搜不到」。
#[test]
fn gbk_note_is_indexed_and_searchable() {
    let dir = vault(&[("utf8.md", "对照：中文笔记\n")]);
    // 「中文笔记」的 GBK 字节。
    fs::write(
        dir.path().join("gbk.md"),
        [0xD6u8, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x0A],
    )
    .expect("写 GBK 文件失败");

    let mut index = SearchIndex::build(dir.path()).expect("建索引失败");
    let result = index.search("中文笔记", None);

    assert_eq!(result.indexed_files, 2, "两篇都该进索引");
    assert_eq!(result.skipped_files, 0);
    assert_eq!(result.total, 2, "GBK 那篇的正文子串必须命中");
    let paths: Vec<&str> = result.hits.iter().map(|hit| hit.path.as_str()).collect();
    assert!(paths.contains(&"gbk.md"), "实际命中：{paths:?}");
    assert!(paths.contains(&"utf8.md"), "实际命中：{paths:?}");
}

#[test]
fn refresh_reads_the_edited_file_from_disk() {
    let dir = vault(&[("a.md", "旧内容\n"), ("b.md", "旧内容\n")]);
    let mut index = SearchIndex::build(dir.path()).expect("建索引失败");

    assert_eq!(index.search("刚打的字", None).total, 0);

    // 模拟 note_flush 落盘。
    write(dir.path(), "a.md", "刚打的字\n");
    assert_eq!(
        index.search("刚打的字", None).total,
        0,
        "不刷新时索引看不见磁盘改动——这是刻意的（B-09）"
    );

    let refreshed = index.search("刚打的字", Some("a.md"));
    assert_eq!(refreshed.total, 1);
    assert_eq!(refreshed.hits[0].path, "a.md");
}

#[test]
fn refresh_of_a_missing_path_is_a_no_op() {
    let dir = vault(&[("a.md", "内容\n")]);
    let mut index = SearchIndex::build(dir.path()).expect("建索引失败");
    let result = index.search("内容", Some("not-in-index.md"));
    assert_eq!(result.total, 1);
}

#[test]
fn empty_query_returns_nothing() {
    let dir = vault(&[("a.md", "任何内容\n")]);
    let result = search(dir.path(), "   ");
    assert_eq!(result.total, 0);
    assert!(result.hits.is_empty());
    assert!(!result.truncated);
}

#[test]
fn a_query_with_a_newline_is_ignored_instead_of_panicking() {
    // 单行输入框打不出换行，但 IPC 可以被直接调用。跨行的命中不属于任何一行，
    // 必须跳过——按行切片时它会让 `hit_end` 越过行尾，那是 panic 而不是错结果。
    let dir = vault(&[("a.md", "alpha\nbeta\n")]);
    let result = search(dir.path(), "alpha\nbeta");
    assert_eq!(result.total, 0);
}

#[test]
fn the_extension_never_counts_as_a_file_name_hit() {
    let dir = vault(&[("note.md", "正文里也有 md\n"), ("alpha.md", "alpha\n")]);

    let by_extension = search(dir.path(), "md");
    assert_eq!(by_extension.total, 1);
    assert!(
        !by_extension.hits[0].name_hit,
        "`note.md` 的命中来自正文：扩展名 `.md` 不该被算成文件名命中"
    );

    let by_stem = search(dir.path(), "alpha");
    assert!(by_stem.hits[0].name_hit);
}

#[test]
fn long_lines_are_windowed_around_the_hit() {
    let padding = "字".repeat(600);
    let content = format!("{padding}needle{padding}\n");
    let dir = vault(&[("a.md", &content)]);

    let result = search(dir.path(), "needle");
    let hit = &result.hits[0];
    assert!(hit.text.chars().count() <= 240, "窗口必须被裁短");
    assert_eq!(slice_utf16(&hit.text, hit.from, hit.to), "needle");
    assert_eq!(hit.column, 600, "行内列号仍然是命中在整行里的位置");
    assert!(hit.from > 0, "命中前面还留着上下文");
}
