//! ki-vault 的验收测试。
//!
//! 这些用例直接对应 `docs/MVP-SCOPE.md` 的 M0 验收标准：
//! 中文/emoji 文件名不崩、忽略 `.git`/`node_modules`/`.knowisle`、
//! 路径必须是 POSIX、路径穿越必须被拒。

use std::fs;
use std::path::Path;

use ki_core::FileNode;
use ki_vault::{read_note, resolve_in_vault, scan_vault, ScanOptions, MAX_SAFE_DEPTH};
use tempfile::TempDir;

/// 把树里所有文件节点的 `path` 收集成排序后的数组，便于断言。
fn collect_files(node: &FileNode) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(children) = &node.children {
        for child in children {
            if child.kind.is_dir() {
                out.extend(collect_files(child));
            } else {
                out.push(child.path.clone());
            }
        }
    }
    out.sort();
    out
}

fn collect_dirs(node: &FileNode) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(children) = &node.children {
        for child in children {
            if child.kind.is_dir() {
                out.push(child.path.clone());
                out.extend(collect_dirs(child));
            }
        }
    }
    out.sort();
    out
}

fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("创建父目录失败");
    }
    fs::write(path, content).expect("写入测试文件失败");
}

#[test]
fn handles_cjk_and_emoji_names() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    write_file(root, "中文笔记.md", "# 你好\n");
    write_file(root, "🚀 火箭.md", "# rocket\n");
    // ZWJ 组合 emoji：一个"字符"由 7 个码点组成，最容易在字符串处理里被切碎
    write_file(root, "👨‍👩‍👧‍👦-family.md", "# family\n");
    write_file(root, "深/层/目录/嵌套笔记.md", "# nested\n");

    let result = scan_vault(root, &ScanOptions::default()).expect("扫描不应失败");

    assert_eq!(result.total_files, 4, "四个文件都应被扫描到");
    assert_eq!(result.skipped, 0, "合法 UTF-8 文件名不应被跳过");

    let files = collect_files(&result.tree);
    assert!(files.contains(&"中文笔记.md".to_string()));
    assert!(files.contains(&"🚀 火箭.md".to_string()));
    assert!(files.contains(&"👨‍👩‍👧‍👦-family.md".to_string()));
    assert!(files.contains(&"深/层/目录/嵌套笔记.md".to_string()));
}

#[test]
fn paths_are_always_posix() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_file(root, "a/b/c.md", "# c\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let files = collect_files(&result.tree);

    assert_eq!(files, vec!["a/b/c.md".to_string()]);
    for path in &files {
        assert!(
            !path.contains('\\'),
            "路径里不允许出现反斜杠：{path}（Windows 上这是最典型的树分裂来源）"
        );
    }
    // 根节点路径恒为空串，前端靠它判断"这是 Vault 根"
    assert_eq!(result.tree.path, "");
}

#[test]
fn ignores_vcs_deps_and_metadata_dirs() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    write_file(root, "正常笔记.md", "# ok\n");
    write_file(root, ".git/config.md", "# 不该出现\n");
    write_file(root, "node_modules/pkg/readme.md", "# 不该出现\n");
    write_file(root, ".knowisle/cache/x.md", "# 不该出现\n");
    write_file(root, "target/debug/y.md", "# 不该出现\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let files = collect_files(&result.tree);

    assert_eq!(files, vec!["正常笔记.md".to_string()]);
    // 被剪掉的目录不应进入树
    let dirs = collect_dirs(&result.tree);
    for ignored in [".git", "node_modules", ".knowisle", "target"] {
        assert!(
            !dirs
                .iter()
                .any(|d| d == ignored || d.starts_with(&format!("{ignored}/"))),
            "{ignored} 不应出现在树里"
        );
    }
}

#[test]
fn keeps_root_even_if_its_name_is_ignored() {
    // 用户完全可能把一个名叫 `target` 的目录选作 Vault。
    // depth()==0 必须放行，否则会出现"选了文件夹却空空如也"。
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("target");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("note.md"), "# hi\n").unwrap();

    let result = scan_vault(&root, &ScanOptions::default()).unwrap();
    assert_eq!(collect_files(&result.tree), vec!["note.md".to_string()]);
}

#[test]
fn prunes_empty_dirs_by_default() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    write_file(root, "有笔记/a.md", "# a\n");
    // 只有图片、没有笔记的目录
    write_file(root, "只有图片/pic.png", "fake-png");
    fs::create_dir_all(root.join("完全空的目录")).unwrap();

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let dirs = collect_dirs(&result.tree);

    assert_eq!(
        dirs,
        vec!["有笔记".to_string()],
        "空目录与纯附件目录都应被剪掉"
    );
}

#[test]
fn can_keep_empty_dirs_when_pruning_disabled() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("空目录")).unwrap();
    write_file(root, "a.md", "# a\n");

    let opts = ScanOptions {
        prune_empty_dirs: false,
        ..ScanOptions::default()
    };
    let result = scan_vault(root, &opts).unwrap();

    assert!(collect_dirs(&result.tree).contains(&"空目录".to_string()));
}

#[test]
fn sorts_dirs_before_files_then_by_name() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    write_file(root, "b.md", "# b\n");
    write_file(root, "A.md", "# A\n");
    write_file(root, "zzz/c.md", "# c\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let children = result.tree.children.as_ref().unwrap();
    let order: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();

    // 目录优先；文件按不区分大小写排序 → A 在 b 之前
    assert_eq!(order, vec!["zzz", "A.md", "b.md"]);
}

#[test]
fn rejects_path_traversal() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_file(root, "a.md", "# a\n");

    let attacks = [
        "../etc/passwd",
        "..\\..\\windows\\win.ini",
        "a/../../b",
        "/etc/passwd",
        "C:\\Windows\\win.ini",
        "\\\\server\\share\\x.md",
    ];

    for attack in attacks {
        let err = resolve_in_vault(root, attack).expect_err(&format!("{attack} 必须被拒绝"));
        let code = format!("{err:?}");
        assert!(
            code.contains("PathEscape"),
            "{attack} 应该返回 PATH_ESCAPE，实际是 {code}"
        );
    }
}

#[test]
fn resolves_normal_relative_path() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_file(root, "深/层/笔记.md", "# hi\n");

    let resolved = resolve_in_vault(root, "深/层/笔记.md").expect("合法路径应能解析");
    assert!(resolved.ends_with("笔记.md"));
}

#[test]
fn root_must_be_directory() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("not-a-dir.md");
    fs::write(&file, "# x\n").unwrap();

    let err = scan_vault(&file, &ScanOptions::default()).expect_err("传文件路径必须报错");
    assert!(format!("{err:?}").contains("NotADirectory"));
}

#[test]
fn read_note_strips_utf8_bom() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Windows 记事本保存 UTF-8 时的典型产物
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice("# 标题\n正文\n".as_bytes());
    fs::write(root.join("bom.md"), &bytes).unwrap();

    let note = read_note(root, "bom.md").expect("读取应成功");
    assert!(
        note.content.starts_with("# 标题"),
        "BOM 必须被剥离，否则首个标题解析会失效：{:?}",
        &note.content[..note.content.len().min(10)]
    );
}

#[test]
fn read_note_rejects_non_utf8() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    // 0xFF 0xFE 是 UTF-16LE BOM 的开头，作为 UTF-8 非法
    fs::write(root.join("gbk.md"), [0xFF, 0xFE, 0x00, 0x41]).unwrap();

    let err = read_note(root, "gbk.md").expect_err("非 UTF-8 必须报错而不是 panic");
    assert!(format!("{err:?}").contains("InvalidEncoding"));
}

#[test]
fn read_note_rejects_traversal() {
    let dir = TempDir::new().unwrap();
    let err = read_note(dir.path(), "../../secret.md").expect_err("穿越必须被拒绝");
    assert!(format!("{err:?}").contains("PathEscape"));
}

#[cfg(unix)]
#[test]
fn does_not_follow_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_file(root, "real/a.md", "# a\n");
    // 指回父目录的软链：跟随会导致无限递归
    symlink(root, root.join("loop")).unwrap();

    let result = scan_vault(root, &ScanOptions::default()).expect("软链不应导致扫描失败");
    assert_eq!(collect_files(&result.tree), vec!["real/a.md".to_string()]);
}

/* ───────────────── 深度上限：极端入参不得让递归失控 ───────────────── */

/// 造一条 `levels` 层目录链，最深处放一篇笔记，返回最深目录的相对路径。
fn deep_fixture(root: &Path, levels: usize) -> String {
    try_deep_fixture(root, levels).expect("创建深层夹具失败")
}

/// 同上，但建不出来时返回 `None` 而不是 panic。
///
/// 用于"比上限还深"的夹具：Windows 未启用长路径时 MAX_PATH 会拦在
/// 约 100 层，那时这是环境限制而非代码缺陷，调用方应当跳过而不是失败。
fn try_deep_fixture(root: &Path, levels: usize) -> Option<String> {
    let mut rel = String::new();
    for i in 0..levels {
        if i > 0 {
            rel.push('/');
        }
        rel.push('d');
    }
    let dir_path = root.join(&rel);
    fs::create_dir_all(&dir_path).ok()?;
    fs::write(dir_path.join("note.md"), "# deep\n").ok()?;
    Some(rel)
}

fn depth_of(node: &FileNode) -> usize {
    node.children
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|child| 1 + depth_of(child))
        .max()
        .unwrap_or(0)
}

/// 深度测试专用参数：关掉剪空目录，否则断言会被"目录被剪掉"污染。
fn opts_with_depth(max_depth: usize) -> ScanOptions {
    ScanOptions {
        max_depth,
        prune_empty_dirs: false,
        ..ScanOptions::default()
    }
}

#[test]
fn extreme_max_depth_keeps_every_level_and_does_not_crash() {
    let dir = TempDir::new().unwrap();
    let deepest = deep_fixture(dir.path(), 10);

    let result =
        scan_vault(dir.path(), &opts_with_depth(usize::MAX)).expect("极端 max_depth 不应导致失败");

    assert_eq!(result.total_files, 1);
    assert_eq!(
        depth_of(&result.tree),
        11,
        "10 层目录 + 最深处的文件 = 11 条边；收敛只该削掉超限深度，不该误伤正常夹具"
    );
    assert!(
        collect_files(&result.tree).contains(&format!("{deepest}/note.md")),
        "最深处的笔记必须仍在树里"
    );
}

#[test]
fn extreme_max_depth_matches_default_behaviour() {
    let dir = TempDir::new().unwrap();
    deep_fixture(dir.path(), 6);

    let with_default = scan_vault(dir.path(), &ScanOptions::default()).unwrap();
    let with_extreme = scan_vault(
        dir.path(),
        &ScanOptions {
            max_depth: usize::MAX,
            ..ScanOptions::default()
        },
    )
    .unwrap();

    assert_eq!(
        collect_files(&with_default.tree),
        collect_files(&with_extreme.tree),
        "在远未触及上限的库上，usize::MAX 与默认值必须完全等价"
    );
    assert_eq!(with_default.total_dirs, with_extreme.total_dirs);
}

#[test]
fn small_max_depth_truncates_the_walk() {
    let dir = TempDir::new().unwrap();
    deep_fixture(dir.path(), 10);

    let result = scan_vault(dir.path(), &opts_with_depth(3)).unwrap();

    assert_eq!(depth_of(&result.tree), 3, "树深必须正好卡在 3");
    assert_eq!(result.total_files, 0, "笔记在第 10 层，应被深度上限截断");
}

#[test]
fn max_depth_zero_yields_an_empty_tree() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "a/b.md", "# b\n");

    let result = scan_vault(dir.path(), &opts_with_depth(0)).unwrap();

    assert_eq!(result.total_files, 0);
    assert_eq!(result.total_dirs, 0);
    assert!(result.tree.children.unwrap_or_default().is_empty());
}

/// 端到端证明 clamp 真的接上了：造一条比 `MAX_SAFE_DEPTH` 更深的链，
/// 传 `usize::MAX`，树深必须被硬砍在 `MAX_SAFE_DEPTH`。
///
/// 这条是唯一能抓住"有人把 `clamp_depth` 删了"的用例。建不出这么深的
/// 夹具时（Windows 未开长路径）跳过——环境限制，不是代码缺陷。
#[test]
fn max_depth_is_clamped_end_to_end() {
    let dir = TempDir::new().unwrap();
    let levels = MAX_SAFE_DEPTH + 8;

    if try_deep_fixture(dir.path(), levels).is_none() {
        eprintln!("跳过：当前文件系统建不出 {levels} 层目录");
        return;
    }

    let result = scan_vault(dir.path(), &opts_with_depth(usize::MAX)).unwrap();

    assert_eq!(
        depth_of(&result.tree),
        MAX_SAFE_DEPTH,
        "请求 {levels} 层，实际必须被砍在 {MAX_SAFE_DEPTH} 层"
    );
    assert_eq!(
        result.total_files, 0,
        "唯一的笔记在第 {levels} 层，超出上限后不该出现在树里"
    );
}
