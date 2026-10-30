//! 忽略规则可配置（B-05 / ADR-014）的验收用例。
//!
//! 七条判据逐条对应施工指令 §4 的前六项（第七项是门禁，不在这里）：
//!
//! 1. 配置文件不存在时，行为与改动前**逐字节相同**（冻结快照）；
//! 2. `extra` 里的名字在树里消失，且**不计入** `skipped`；
//! 3. `negate` 能救回内置名字；`.git` / `.knowisle` 救不回来，且给得出理由；
//! 4. 三种「写坏」都退回内置默认值并可看见（不存在 / 语法错 / 语义错）；
//! 5. 配置写坏时库仍然打得开；
//! 6. 删掉配置文件 → 树回到默认（与第 1 项同一份快照）；
//! 7. `max_entries` / `truncated` 语义不动。
//!
//! # 为什么断言的是「快照」而不是几个 `contains`
//!
//! `contains` 只能证明「该在的在」，证明不了「不该在的不在」。而这一刀最危险的
//! 回归恰恰是**多忽略了一个目录**——那种错误会让 `contains` 全绿、用户少一片笔记。
//! 所以第 1 项把整棵树（路径 + 字节数 + 目录）连同 `skipped` 一起冻成一份字符串。

use std::fs;
use std::path::Path;

use ki_core::FileNode;
use ki_vault::ignore::config_path;
use ki_vault::{scan_vault, IgnoreSource, ScanOptions};
use tempfile::TempDir;

/* ───────────────────────── 夹具 ───────────────────────── */

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).expect("建父目录");
    fs::write(path, body).expect("写夹具");
}

/// 写一份配置。父目录（`.knowisle`）不存在时自动建。
fn write_config(root: &Path, text: &str) {
    let path = config_path(root);
    fs::create_dir_all(path.parent().unwrap()).expect("建 .knowisle");
    fs::write(path, text).expect("写配置");
}

/// 把树压成「路径 + 字节数」的稳定表示。
///
/// **不含 mtime**：那是文件自己的时钟，不是树的结构。含它的话快照每次跑都会变，
/// 而用例想钉住的是「哪些条目在、哪些不在」。
fn flatten(node: &FileNode, out: &mut Vec<String>) {
    for child in node.children.as_deref().unwrap_or(&[]) {
        if child.kind.is_dir() {
            out.push(format!("dir:{}", child.path));
            flatten(child, out);
        } else {
            out.push(format!("file:{}:{}", child.path, child.size));
        }
    }
}

/// 一次扫描的冻结快照：树 + 计数三件套 + `skipped`。
///
/// `ignore_rules` 与 `elapsed_ms` 刻意不在里面：前者是本刀新增的**诊断**字段
/// （它变了不代表树变了），后者每次都不一样。判据 1 要证明的是「树没动」。
///
/// `total_files` / `total_dirs` 在 M7 第十四刀补进来：改名那一刀最危险的
/// 回归不是树结构变了，而是计数口径被顺手改了（例如把目录从 `total_files`
/// 里扣掉）。三个数一起冻住，才叫「逐字节相同」。
fn snapshot(root: &Path, opts: &ScanOptions) -> String {
    let result = scan_vault(root, opts).expect("扫描不应失败");
    let mut entries = Vec::new();
    flatten(&result.tree, &mut entries);
    entries.sort();
    serde_json::json!({
        "entries": entries,
        "total_files": result.total_files,
        "total_dirs": result.total_dirs,
        "skipped": result.skipped,
        "detail": result.skipped_detail,
        "truncated": result.skipped_truncated,
    })
    .to_string()
}

/// 判据 1 / 6 的基准：**在改动之前**用旧代码扫出来的结果，逐字抄在这里。
///
/// 抄的是「路径 + 字节数」，因为这三样才是树。任何一条变了都说明默认行为动了。
const DEFAULT_ENTRIES: &[&str] = &[
    "dir:a",
    "dir:a/b",
    "dir:assets",
    "dir:my-notes",
    "dir:中文",
    "dir:空目录",
    "file:a/b/c.markdown:9",
    "file:my-notes/keep.md:9",
    "file:中文/笔记.md:9",
    "file:正常笔记.md:9",
];

fn expected_default_snapshot() -> String {
    serde_json::json!({
        "entries": DEFAULT_ENTRIES,
        "total_files": 4,
        "total_dirs": 6,
        "skipped": 0,
        "detail": [],
        "truncated": false,
    })
    .to_string()
}

/// 默认夹具：**故意把每一种误伤都摆进来**。
///
/// - `logseq/`、`dist/`、`node_modules/`、`my-notes/dist/` 四条内置规则命中；
/// - `my-notes/keep.md` 与 `空目录` 证明「剪枝没有连坐到旁边」；
/// - `assets/pic.png` 证明非笔记文件不影响目录的存在。
fn build_default_fixture(root: &Path) {
    write(root, "正常笔记.md", "# 正常\n");
    write(root, "中文/笔记.md", "# 中文\n");
    write(root, "a/b/c.markdown", "# 深层\n");
    write(root, "logseq/pages/a.md", "# logseq\n");
    write(root, "dist/x.md", "# dist\n");
    write(root, "node_modules/y.md", "# nm\n");
    write(root, "my-notes/dist/z.md", "# 嵌套 dist\n");
    write(root, "my-notes/keep.md", "# 保留\n");
    write(root, ".git/config.md", "# git\n");
    write(root, ".knowisle/index.sqlite", "not a note");
    write(root, "assets/pic.png", "png");
    fs::create_dir_all(root.join("空目录")).unwrap();
    write(root, ".DS_Store", "junk");
}

fn opts_from(source: IgnoreSource) -> ScanOptions {
    ScanOptions {
        ignore: source,
        ..ScanOptions::default()
    }
}

fn files(root: &Path, opts: &ScanOptions) -> Vec<String> {
    let result = scan_vault(root, opts).expect("扫描");
    let mut out = Vec::new();
    flatten(&result.tree, &mut out);
    out.retain(|line| line.starts_with("file:"));
    out.sort();
    out
}

/* ═════════════ 1. 没有配置文件 = 与改动前逐字节相同 ═════════════ */

#[test]
fn missing_config_changes_nothing() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);

    assert!(
        !config_path(root).exists(),
        "前提：这份夹具里没有配置文件"
    );

    // 「今天的」路径（BuiltinOnly）与「应用默认」路径（VaultConfig，缺文件）
    // 必须与冻结快照逐字节相同。三者相等才算真的没动。
    let today = snapshot(root, &opts_from(IgnoreSource::BuiltinOnly));
    let app = snapshot(root, &ScanOptions::default());
    assert_eq!(today, expected_default_snapshot(), "BuiltinOnly 与冻结快照不符");
    assert_eq!(app, expected_default_snapshot(), "缺配置时必须逐字节相同");
    assert_eq!(today, app);
}

/* ═════════════ 2. extra：消失，且不计入 skipped ═════════════ */

/// 面板要写出「去改哪个文件」，所以路径必须跟着规则回来——**文件不存在时也要有**。
///
/// 让前端自己拼 `<root>/.knowisle/config.toml` 等于把 `METADATA_DIR` 复制出
/// 第四份，而这个名字已经有三处在用同一个常量了。
#[test]
fn config_path_is_reported_even_when_the_file_is_absent() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);
    assert!(!config_path(root).exists());

    let app = scan_vault(root, &ScanOptions::default()).unwrap();
    let reported = app
        .ignore_rules
        .config_path
        .expect("VaultConfig 必须报告「去看过哪个文件」");
    assert!(reported.ends_with("config.toml"), "{reported}");
    assert!(
        reported.replace('\\', "/").ends_with(".knowisle/config.toml"),
        "路径要落在元数据目录里：{reported}"
    );

    // 「只看内置名单」没去看文件，所以是 None——这两种状态必须分得开。
    let builtin = scan_vault(root, &opts_from(IgnoreSource::BuiltinOnly)).unwrap();
    assert_eq!(builtin.ignore_rules.config_path, None);
}

#[test]
fn extra_hides_a_directory_without_counting_it_as_skipped() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);
    write_config(root, "[ignore]\nextra = [\"my-notes\"]\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let mut entries = Vec::new();
    flatten(&result.tree, &mut entries);

    assert!(
        !entries.iter().any(|line| line.contains("my-notes")),
        "被 extra 命中的目录整棵都不该出现：{entries:?}"
    );
    // 红线：忽略不是跳过。数出来就说明它混进了「我试了，没成」那一栏。
    assert_eq!(result.skipped, 0, "忽略不许计入 skipped");
    assert!(result.skipped_detail.is_empty(), "也不许进跳过清单");
    assert!(!result.skipped_truncated);

    // 规则本身要能查：用户看得见「为什么这个目录不在树里」。
    let rules = &result.ignore_rules;
    assert_eq!(rules.extra, vec!["my-notes"]);
    assert!(rules.negate.is_empty());
    assert!(rules.config_error.is_none(), "合法配置不该有错误");
    assert_eq!(
        rules.builtin.len(),
        ki_core::ignore::BUILTIN_DIRS.len(),
        "内置名单要原样带出来，negate 不从里面删"
    );
}

/* ═════════════ 3. negate：能救回 logseq，救不回 .git ═════════════ */

#[test]
fn negate_brings_back_a_builtin_name() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);
    write_config(root, "[ignore]\nnegate = [\"logseq\"]\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let mut entries = Vec::new();
    flatten(&result.tree, &mut entries);

    assert!(
        entries.contains(&"file:logseq/pages/a.md:9".to_string()),
        "negate 之后 logseq 下的笔记必须回来：{entries:?}"
    );
    // 其它内置仍然生效——negate 只减一条，不是「关掉整张名单」。
    assert!(
        !entries.iter().any(|line| line.contains("dist/")),
        "negate 只影响被点名的那个名字：{entries:?}"
    );
    assert_eq!(result.ignore_rules.negate, vec!["logseq"]);
    assert_eq!(result.skipped, 0);
}

#[test]
fn git_and_metadata_dir_can_never_be_negated() {
    for name in [".git", ".knowisle"] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        build_default_fixture(root);
        write_config(root, &format!("[ignore]\nnegate = [\"{name}\"]\n"));

        let result = scan_vault(root, &ScanOptions::default()).unwrap();
        let mut entries = Vec::new();
        flatten(&result.tree, &mut entries);

        assert!(
            !entries.iter().any(|line| line.contains(".git/")),
            "{name} 必须仍在树外：{entries:?}"
        );
        assert_eq!(result.skipped, 0);

        // 不是「静默忽略你写的 negate」，而是一条**可读的拒绝理由**。
        let error = result
            .ignore_rules
            .config_error
            .as_ref()
            .unwrap_or_else(|| panic!("{name} 被 negate 时必须报配置错误"));
        assert!(
            error.message.contains("永远被忽略") && error.message.contains(".git"),
            "拒绝理由要点名并解释：{}",
            error.message
        );
        assert_eq!(error.line, Some(2), "行号要指到那一行");
        assert!(
            error.path.ends_with("config.toml"),
            "路径要指到文件：{}",
            error.path
        );
        // 写坏的配置整体退回内置默认：规则里不该留下这次 negate。
        assert!(
            result.ignore_rules.negate.is_empty(),
            "被拒绝的 negate 不能半生效"
        );
    }
}

/* ═════════════ 4 / 5. 三种写坏：退回默认 + 可看见 ═════════════ */

#[test]
fn broken_config_falls_back_to_builtin_and_is_visible() {
    // 语法错：数组没闭合。
    let syntax = "[ignore]\nextra = [\"我的笔记\"\n";
    // 语义错：字符串而不是数组。
    let semantic = "[ignore]\nextra = \"dist\"\n";

    for (label, text, line) in [("语法错", syntax, 2u32), ("语义错", semantic, 2u32)] {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        build_default_fixture(root);
        write_config(root, text);

        // 判据 5：库仍然打得开——`scan_vault` 返回 `Ok`，不是 `Err`。
        let result = scan_vault(root, &ScanOptions::default())
            .unwrap_or_else(|err| panic!("{label} 不该让扫描失败：{err:?}"));

        let mut entries = Vec::new();
        flatten(&result.tree, &mut entries);
        assert!(
            entries.contains(&"file:my-notes/keep.md:9".to_string()),
            "{label} 时树必须与默认一致：{entries:?}"
        );

        let error = result
            .ignore_rules
            .config_error
            .as_ref()
            .unwrap_or_else(|| panic!("{label} 必须给一条可读错误"));
        assert_eq!(error.line, Some(line), "{label} 的行号：{}", error.message);
        assert!(
            error.message.chars().count() >= 12,
            "{label} 的错误要说人话，不能是类型名：{}",
            error.message
        );
        assert!(error.path.ends_with("config.toml"));
    }
}

/// 语义错的那一句必须说清「这里是一个字符串」——否则用户不知道 `= "dist"`
/// 和 `= ["dist"]` 差在哪。
#[test]
fn semantic_error_says_what_was_wrong() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);
    write_config(root, "[ignore]\nextra = \"dist\"\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let message = result.ignore_rules.config_error.unwrap().message;
    assert!(
        message.contains("必须是字符串数组") && message.contains("字符串"),
        "{message}"
    );
}

/// 配置读不出来（不是不存在）也要退回默认，并且说得出「读不出来」。
#[test]
fn unreadable_config_falls_back_without_failing_the_scan() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);
    // 同名目录：`read_to_string` 会报 IsADirectory（Windows）/ IsADirectory（Unix）。
    fs::create_dir_all(config_path(root)).unwrap();

    let result = scan_vault(root, &ScanOptions::default()).expect("配置读不出来不许让库打不开");
    let error = result.ignore_rules.config_error.expect("要给一条错误");
    assert!(error.line.is_none(), "整个文件级失败时没有行号");
    assert!(error.path.ends_with("config.toml"));
    assert_eq!(result.total_files, 4, "树退回默认");
}

/* ═════════════ 6. 删掉配置 → 回到默认 ═════════════ */

#[test]
fn deleting_the_config_restores_the_default_tree() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);

    write_config(root, "[ignore]\nextra = [\"my-notes\"]\nnegate = [\"logseq\"]\n");
    let with_config = snapshot(root, &ScanOptions::default());
    assert_ne!(
        with_config,
        expected_default_snapshot(),
        "前提：这份配置确实改变了树"
    );

    fs::remove_file(config_path(root)).unwrap();
    assert_eq!(
        snapshot(root, &ScanOptions::default()),
        expected_default_snapshot(),
        "删掉配置后必须与第 1 项的快照逐字节相同"
    );
}

/* ═════════════ 7. max_entries / truncated 语义不动 ═════════════ */

#[test]
fn truncated_still_means_the_entry_cap() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for index in 0..8 {
        write(root, &format!("n{index}.md"), "# n\n");
    }
    // 被忽略的条目**不进 `flat`**，因此不占上限额度：`node_modules` 里
    // 这 5 篇如果算进额度，`total_files` 会变成 2（第 4 条就撞上限）。
    for index in 0..5 {
        write(root, &format!("node_modules/p{index}.md"), "# 依赖\n");
    }
    write_config(root, "[ignore]\nextra = [\"nothing-here\"]\n");

    let opts = ScanOptions {
        max_entries: 3,
        ..ScanOptions::default()
    };
    let result = scan_vault(root, &opts).unwrap();
    assert!(result.truncated, "达到条目上限必须置 truncated");
    assert_eq!(result.total_files, 3, "上限仍然数的是条目");
    assert_eq!(result.limit_entries, 3, "生效上限必须从结果里带出来");
    // 忽略规则不改变这条语义：`extra` 里那个不存在的名字不该影响上限。
    assert_eq!(result.ignore_rules.extra, vec!["nothing-here"]);
}

/* ═════════════ 配置改了要重扫才生效 ═════════════ */

/// 扫描**不缓存**规则：改完配置再扫一次就能看见变化。
///
/// 这条反过来证明「UI 上必须写『改配置后需要重新扫描』」不是偷懒：
/// 应用里没有 watcher（ADR-011 已经否过一次），所以不重扫就真的看不到。
#[test]
fn rules_are_read_again_on_every_scan() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);

    assert!(files(root, &ScanOptions::default()).contains(&"file:my-notes/keep.md:9".to_string()));

    write_config(root, "[ignore]\nextra = [\"my-notes\"]\n");
    assert!(
        !files(root, &ScanOptions::default()).contains(&"file:my-notes/keep.md:9".to_string()),
        "第二次扫描必须读到新配置"
    );

    write_config(root, "[ignore]\nextra = []\n");
    assert!(
        files(root, &ScanOptions::default()).contains(&"file:my-notes/keep.md:9".to_string()),
        "清空 extra 之后必须回来"
    );
}

/* ═════════════ 大小写：三平台同一条判定 ═════════════ */

/// 配置里的名字走同一条 `eq_ignore_ascii_case`，Windows 不敏感、Linux 也**故意**
/// 不敏感——一份配置要在三平台行为一致。
#[test]
fn rule_names_are_case_insensitive_everywhere() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    build_default_fixture(root);
    write_config(root, "[ignore]\nextra = [\"MY-NOTES\"]\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let mut entries = Vec::new();
    flatten(&result.tree, &mut entries);
    assert!(
        !entries.iter().any(|line| line.contains("my-notes")),
        "大小写不同的同名目录也要命中：{entries:?}"
    );
}
