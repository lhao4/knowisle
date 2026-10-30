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

/// `path` 的任意一段是否等于 `name`（大小写不敏感）。
///
/// 忽略名单是**按目录名**生效的，所以断言必须逐段比较。
/// 用 `starts_with("logseq/")` 这种前缀判断会漏掉嵌套的 `子库/logseq/`。
fn has_component(path: &str, name: &str) -> bool {
    path.split('/').any(|part| part.eq_ignore_ascii_case(name))
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
            !dirs.iter().any(|d| has_component(d, ignored)),
            "{ignored} 不应出现在树里，实际目录：{dirs:?}"
        );
    }
}

/// Obsidian / Logseq 是最主要的迁移入口，它们的元数据目录里塞满了 `.md`：
/// 插件自带的 README、回收站里的笔记、`logseq/bak/` 里的全量历史副本。
/// 不剪掉的话，用户第一次打开自己的库看到的是一堆别人的文档和重复笔记。
#[test]
fn ignores_obsidian_and_logseq_metadata() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    write_file(root, "我的笔记.md", "# ok\n");
    write_file(root, "日记/2026-10-24.md", "# 今天\n");

    // Obsidian：插件 README、workspace 快照、回收站
    write_file(root, ".obsidian/plugins/foo/README.md", "# 插件文档\n");
    write_file(root, ".obsidian/workspace.md", "# 不该出现\n");
    write_file(root, ".trash/删掉的笔记.md", "# 不该出现\n");
    // 忽略按目录名生效，不只在根下一层
    write_file(root, "子库/.obsidian/app.md", "# 不该出现\n");

    // Logseq：`logseq/bak/` 是 pages 的全量历史副本，不剪会整片重复
    write_file(root, "logseq/bak/我的笔记/2026-10-24.md", "# 旧副本\n");
    write_file(root, "logseq/config.md", "# 不该出现\n");
    write_file(root, ".logseq/settings.md", "# 不该出现\n");

    // Syncthing 的版本副本
    write_file(root, ".stversions/我的笔记.md", "# 不该出现\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();

    assert_eq!(
        collect_files(&result.tree),
        vec!["我的笔记.md".to_string(), "日记/2026-10-24.md".to_string()],
        "元数据目录里的 .md 一篇都不该进树"
    );

    let dirs = collect_dirs(&result.tree);
    for ignored in [".obsidian", ".trash", ".logseq", "logseq", ".stversions"] {
        assert!(
            !dirs.iter().any(|d| has_component(d, ignored)),
            "{ignored} 不应出现在树里（含嵌套层级），实际目录：{dirs:?}"
        );
    }
}

/// 忽略只针对**目录**。叫 `logseq.md` / `target.md` 的笔记是合法文件，
/// 不该被目录名单连坐——否则用户给自己的笔记起名都要绕开工具的名字。
#[test]
fn ignored_dir_names_do_not_shadow_note_files() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    write_file(root, "logseq.md", "# 关于 logseq 的笔记\n");
    write_file(root, "target.md", "# 关于 target 的笔记\n");
    write_file(root, ".obsidian.md", "# 关于 obsidian 的笔记\n");

    let result = scan_vault(root, &ScanOptions::default()).unwrap();

    assert_eq!(
        collect_files(&result.tree),
        vec![
            ".obsidian.md".to_string(),
            "logseq.md".to_string(),
            "target.md".to_string(),
        ],
        "目录名单不该影响同名的 .md 文件"
    );
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

/// 默认**不剪**空目录：磁盘上有什么，树里就该有什么。
///
/// 这条锁的是"新建文件夹不会凭空消失"。M0 曾默认剪掉（为了侧栏干净），
/// 但 M2 一加"新建文件夹"，用户建完的空目录会立刻消失，观感就是丢数据。
/// 默认值因此提前翻成 `false`，不留地雷。
#[test]
fn keeps_empty_dirs_by_default() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    write_file(root, "有笔记/a.md", "# a\n");
    // 只有图片、没有笔记的目录
    write_file(root, "只有图片/pic.png", "fake-png");
    fs::create_dir_all(root.join("完全空的目录")).unwrap();

    let result = scan_vault(root, &ScanOptions::default()).unwrap();
    let dirs = collect_dirs(&result.tree);

    for expected in ["有笔记", "只有图片", "完全空的目录"] {
        assert!(
            dirs.iter().any(|d| d == expected),
            "{expected} 默认应保留，实际目录：{dirs:?}"
        );
    }
    assert_eq!(dirs.len(), 3, "不该多出别的目录：{dirs:?}");
}

/// 剪枝是**显式**选项，不是默认行为。开着它时，空目录与纯附件目录都要被剪掉。
#[test]
fn prunes_empty_dirs_when_explicitly_enabled() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    write_file(root, "有笔记/a.md", "# a\n");
    write_file(root, "只有图片/pic.png", "fake-png");
    fs::create_dir_all(root.join("完全空的目录")).unwrap();

    let opts = ScanOptions {
        prune_empty_dirs: true,
        ..ScanOptions::default()
    };
    let result = scan_vault(root, &opts).unwrap();

    assert_eq!(
        collect_dirs(&result.tree),
        vec!["有笔记".to_string()],
        "开启剪枝后，空目录与纯附件目录都应被剪掉"
    );
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

    // 只放跨平台成立的形态：`..` 与绝对路径在任何平台上都是越界语法。
    // Windows 专有的反斜杠 / 盘符 / UNC 见下一个用例。
    let attacks = ["../etc/passwd", "a/../../b", "/etc/passwd"];

    for attack in attacks {
        let err = resolve_in_vault(root, attack).expect_err(&format!("{attack} 必须被拒绝"));
        let code = format!("{err:?}");
        assert!(
            code.contains("PathEscape"),
            "{attack} 应该返回 PATH_ESCAPE，实际是 {code}"
        );
    }
}

/// 反斜杠、盘符、UNC 只在 Windows 上是路径分隔语法。
///
/// 在 Unix 上它们只是普通文件名字符：`C:\Windows\win.ini` 是一个合法文件名，
/// 校验会放行，随后因为文件不存在报 `Io`。把这三条塞进跨平台用例里断言
/// `PathEscape`，会在 Linux / macOS 上稳定假失败——这是第一次三平台 CI 抓出来的。
#[cfg(windows)]
#[test]
fn rejects_windows_style_paths() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_file(root, "a.md", "# a\n");

    let attacks = [
        "..\\..\\windows\\win.ini",
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

/// `FF FE` 是 UTF-16LE 的 BOM。M7 第八刀之前这条用例断言「非 UTF-8 必须
/// 报错」；现在它断言**按 BOM 解出来是什么**——契约从「认不出就拒绝」
/// 变成「认得出就解开」，这一行就是那次变更的落点。
///
/// 后面两字节 `00 41` 在 UTF-16LE 里是**低字节在前**，所以是 `U+4100`
/// （`䄀`）而不是 `U+0041`。要 `A` 得写成 `41 00`——别照字面想当然。
#[test]
fn read_note_decodes_utf16le_with_bom() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(root.join("u16.md"), [0xFF, 0xFE, 0x00, 0x41]).unwrap();

    let note = read_note(root, "u16.md").expect("带 BOM 的 UTF-16LE 必须能读");
    assert_eq!(note.content, "\u{4100}");
    assert_eq!(note.decoded_from.as_deref(), Some("UTF-16LE"));
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

    // 软链是一个**目录项**，既不是目录也不是普通文件——它必须可定位。
    // 「树里少了 1 个」是 M0 的行为；本刀之后还要能说出「少的是这一个」。
    assert_eq!(result.skipped, 1);
    assert_eq!(result.skipped_detail.len(), 1);
    assert_eq!(result.skipped_detail[0].path.as_deref(), Some("loop"));
    assert_eq!(
        result.skipped_detail[0].reason,
        ki_core::ScanSkipReason::NotARegularFile
    );
    assert!(!result.skipped_truncated);
}

/* ───────────────── 跳过清单：每一类都要能定位 ───────────────── */

/// 60 个非 UTF-8 文件名：总数精确 60、清单 50 条、`truncated` 为真。
///
/// # 为什么必须是真文件而不是构造出来的 `ScanSkipEntry`
///
/// `SkipLog` 的上限逻辑有单测（`scan.rs` 的 `tests` 模块），但那只证明
/// 「收集器会截断」。这条证明的是**扫描器真的把 60 个这样的目录项喂给了它**——
/// 少一次 `record` 调用，两边的数字就对不上，而那种漏法是编译器看不见的。
///
/// # 为什么只能在 Unix 上跑
///
/// NTFS 的文件名是 UTF-16，Rust 的 `OsStr::to_str()` 只在「不成对的代理项」
/// 时返回 `None`，而那种名字 Win32 根本建不出来。所以 Windows 上这一档
/// **不可构造**——不是测试写不出来，是操作系统不存在这种名字。
#[cfg(unix)]
#[test]
fn sixty_non_utf8_names_are_counted_but_listed_only_fifty() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    const COUNT: usize = 60;

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_file(root, "normal.md", "# 正常\n");

    for index in 0..COUNT {
        // GBK 的「中文」= D6 D0 CE C4，单独拿出来不是合法 UTF-8。
        let mut bytes = vec![0xd6, 0xd0, 0xce, 0xc4, b'-'];
        bytes.extend_from_slice(format!("{index:02}").as_bytes());
        bytes.extend_from_slice(b".md");
        let name = OsStr::from_bytes(&bytes);
        fs::write(root.join(name), b"# gbk\n").unwrap();
    }

    let result = scan_vault(root, &ScanOptions::default()).expect("非 UTF-8 文件名不该让扫描失败");

    assert_eq!(result.total_files, 1, "只有 normal.md 进树");
    assert_eq!(result.skipped, COUNT, "总数必须精确");
    assert_eq!(result.skipped_detail.len(), 50, "清单只保留前 50 条");
    assert!(result.skipped_truncated, "清单被截断时必须为真");

    for entry in &result.skipped_detail {
        assert_eq!(entry.reason, ki_core::ScanSkipReason::NonUtf8Name);
        // 红线：这一档拿不到真路径，必须带「名字含无法显示的字符」的标记。
        // 少了它，用户会照着 U+FFFD 去改名，然后找不到文件。
        assert!(entry.path_lossy, "lossy 路径必须被标出来：{entry:?}");
        let path = entry.path.as_deref().expect("lossy 之后仍然要有一行");
        assert!(path.ends_with(".md"), "扩展名要能看见：{path}");
        assert!(
            path.contains('\u{FFFD}'),
            "这条用例的前提就是名字里有无法表示的字节：{path}"
        );
    }
}

/// 无读权限的目录：扫描侧计一条 `Permission`，并给出路径。
#[cfg(unix)]
#[test]
fn unreadable_directory_is_reported_with_its_path() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_file(root, "ok.md", "# ok\n");
    fs::create_dir_all(root.join("locked")).unwrap();
    write_file(root, "locked/inside.md", "# 读不到\n");

    let locked = root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let result = scan_vault(root, &ScanOptions::default());

    // 无论断言成败都要把权限还原，否则 TempDir 的 Drop 删不掉它。
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    let result = result.expect("一个目录没权限不该让整次扫描失败");

    assert_eq!(result.total_files, 1, "读不到的目录里那篇不该进树");
    assert_eq!(result.skipped, 1, "目录本身计一条");
    assert_eq!(
        result.skipped_detail[0].path.as_deref(),
        Some("locked"),
        "路径必须指到那个目录，而不是它里面的文件"
    );
    assert_eq!(
        result.skipped_detail[0].reason,
        ki_core::ScanSkipReason::Permission
    );
    assert!(!result.skipped_detail[0].path_lossy, "这个名字是合法 UTF-8");
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

/* ═════════════ 扫描上限是安全阀，不是业务承诺（ADR-015） ═════════════ */

fn opts_with_cap(max_entries: usize) -> ScanOptions {
    ScanOptions {
        max_entries,
        ..ScanOptions::default()
    }
}

/// 判据 2：`limit_entries` 是**实际生效**的上限，不是常量 20000。
///
/// 反例是它曾经的样子：横幅把 20000 写死在 `App.tsx` 里，改了默认值之后
/// 横幅继续撒谎。所以这里必须传一个非默认值，并断言结果原样带回来。
#[test]
fn truncated_reports_the_effective_limit() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for index in 0..8 {
        write_file(root, &format!("n{index}.md"), "# n\n");
    }

    let capped = scan_vault(root, &opts_with_cap(3)).unwrap();
    assert!(capped.truncated);
    assert_eq!(capped.total_files, 3);
    assert_eq!(capped.limit_entries, 3, "必须是生效值，不是默认值");

    // 不截断时它也是真实值：描述「最多愿意收多少条」，不是「收了多少条」。
    let full = scan_vault(root, &ScanOptions::default()).unwrap();
    assert!(!full.truncated);
    assert_eq!(
        full.limit_entries,
        ScanOptions::default().max_entries,
        "未截断时也要带出本次生效的上限"
    );
}

/// 判据 6：截断会制造**假空目录**，这是已知且被说出的行为。
///
/// DFS 中途 `break` 时，边界那个目录已经进了 `flat`，它的孩子一个都没进；
/// `build_tree` 只按 `flat` 里有的条目组树，于是它成了一个空目录。而
/// `prune_empty_dirs` 默认 `false`，所以它会显示出来，且与真的空目录
/// 一模一样。
///
/// # 为什么不能「修」
///
/// 修法只有两条，都比病灶贵：① 给 `FileNode` 加字段标记边界（它是
/// serde / IPC / 前端树的核心结构）；② 截断后再遍历一次数「少了多少篇」
/// （ADR-014 决策五同款的陷阱）。所以本刀把它钉成用例 + 横幅里说一句。
///
/// 横幅那一半在 `apps/desktop/src/lib/truncation.test.ts`（Rust 侧不渲染
/// 文案）；两半合起来才是判据 6。e2e 显式 SKIP：造 20000 个文件 ≈ 80s，
/// 比整轮 e2e 还贵（B-19）。
#[test]
fn the_boundary_dir_can_look_empty_and_that_is_known() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for index in 0..3 {
        write_file(root, &format!("A/n{index}.md"), "# A\n");
    }
    write_file(root, "B/b.md", "# B\n");

    // 走查顺序（`sort_by_file_name`）：A → A/n0 → A/n1 → A/n2 → B → B/b。
    // 上限 1 = 收下 A 的目录项之后立刻停，A 的三个孩子一个都没进 `flat`。
    let result = scan_vault(root, &opts_with_cap(1)).unwrap();

    assert!(result.truncated);
    assert_eq!(result.total_files, 0, "A 的孩子一个都没扫到");
    assert_eq!(result.total_dirs, 1, "A 自己进了树");

    let a = &result.tree.children.as_ref().unwrap()[0];
    assert_eq!(a.path, "A");
    assert!(
        a.is_empty_dir(),
        "边界目录在树里必须显示为空——这是病灶，不是 bug"
    );
    assert_eq!(
        fs::read_dir(root.join("A")).unwrap().count(),
        3,
        "磁盘上它并不是空的"
    );
}
