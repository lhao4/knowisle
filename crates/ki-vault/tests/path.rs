//! `path` 模块的安全边界测试：软链逃逸、读写边界。
//!
//! 平台差异与链接夹具见 `tests/common/mod.rs`。

mod common;

use std::fs;
use std::path::Path;

use common::{skip_unless, try_link_dir, try_link_file};
use ki_vault::{resolve_for_write, resolve_in_vault, ResolvedVault};
use tempfile::TempDir;

/* ───────────────────── 读路径：逃逸必须被拒绝 ───────────────────── */

#[test]
fn symlink_to_outside_is_rejected() {
    let vault = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("secret.md"), "SECRET").unwrap();

    skip_unless!(
        try_link_dir(&vault.path().join("link"), outside.path()),
        "目录链接"
    );

    let err =
        resolve_in_vault(vault.path(), "link/secret.md").expect_err("指向库外的链接必须被拒绝");
    assert!(
        format!("{err:?}").contains("PathEscape"),
        "错误码应为 PathEscape，实际 {err:?}"
    );
}

#[test]
fn nested_symlink_escape_is_rejected() {
    // 链接在中间层而非顶层：a/link -> 库外
    let vault = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("secret.md"), "SECRET").unwrap();
    fs::create_dir_all(vault.path().join("a")).unwrap();

    skip_unless!(
        try_link_dir(&vault.path().join("a").join("link"), outside.path()),
        "目录链接"
    );

    let err =
        resolve_in_vault(vault.path(), "a/link/secret.md").expect_err("中间层链接也必须被拒绝");
    assert!(format!("{err:?}").contains("PathEscape"), "实际 {err:?}");
}

/// 锁定「`starts_with` 是组件比较而非字符串前缀比较」。
///
/// `vault` 与 `vault-evil` 共享字符串前缀，任何用 `str::starts_with` 实现
/// 越界检查的代码都会在这里放行一个库外文件。
#[test]
fn sibling_directory_with_shared_prefix_is_rejected() {
    let base = TempDir::new().unwrap();
    let vault = base.path().join("vault");
    let evil = base.path().join("vault-evil");
    fs::create_dir_all(&vault).unwrap();
    fs::create_dir_all(&evil).unwrap();
    fs::write(evil.join("evil.md"), "EVIL").unwrap();

    skip_unless!(try_link_dir(&vault.join("link"), &evil), "目录链接");

    let err = resolve_in_vault(&vault, "link/evil.md").expect_err("兄弟目录必须被拒绝");
    assert!(format!("{err:?}").contains("PathEscape"), "实际 {err:?}");
}

/// 库内软链**同样**被拒绝：可见边界与可读边界必须是同一条线。
///
/// 这条用例是 B-12 的结论，此前它的断言是反的（库内链接放行）。
/// 当时不构成逃逸——目标本来就在库里——但扫描器 `follow_links(false)`
/// 根本看不见 `link/`，于是 `note_read("link/ok.md")` 读得通、侧栏里却没有这个目录。
/// 一旦有了编辑器，这就从「读得到但看不见」升级成「改得到但看不见」。
///
/// 如果将来要支持软链，必须**两边一起改**（扫描器显示链接 + 解析器放行），
/// 而不是只放开一边。
#[test]
fn symlink_to_inside_is_rejected_too() {
    let vault = TempDir::new().unwrap();
    fs::create_dir_all(vault.path().join("sub")).unwrap();
    fs::write(vault.path().join("sub").join("ok.md"), "INSIDE").unwrap();

    skip_unless!(
        try_link_dir(&vault.path().join("link"), &vault.path().join("sub")),
        "目录链接"
    );

    let err = resolve_in_vault(vault.path(), "link/ok.md")
        .expect_err("库内软链也必须拒绝，否则可见边界与可读边界不一致");
    assert!(format!("{err:?}").contains("PathEscape"), "实际 {err:?}");

    // 真实路径仍然读得到——拒绝的是"穿过链接"，不是"读这个文件"。
    let real = resolve_in_vault(vault.path(), "sub/ok.md").expect("真实路径必须放行");
    assert_eq!(fs::read_to_string(&real).unwrap(), "INSIDE");
}

#[test]
fn symlink_loop_fails_without_panic() {
    let vault = TempDir::new().unwrap();
    let a = vault.path().join("a");
    let b = vault.path().join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();

    skip_unless!(try_link_dir(&a.join("to_b"), &b), "目录链接");
    skip_unless!(try_link_dir(&b.join("to_a"), &a), "目录链接");

    // 环不该 panic，也不该返回库外路径；具体是 IO 错误即可
    let result = resolve_in_vault(vault.path(), "a/to_b/to_a/to_b/x.md");
    assert!(result.is_err(), "软链环必须失败，实际 {result:?}");
}

/* ───────────────── 批量读取：句柄的缓存键与库根规范化 ───────────────── */

/// 缓存键必须是**规范化后的父目录相对路径**，不是调用方传来的原始字符串。
///
/// 判据不用「内部命中次数」（那是实现细节），而是拿「父目录在批次中途变成
/// junction」去观测：
///
/// - 键规范化过 → `./a` 命中 `a` 那一格 → 沿用第一次的结论 → 放行；
/// - 键用原始串 → 未命中 → 重跑第一道闸 → `PathEscape`。
///
/// 这条同时钉住了「缓存只在单批次内有效」这个语义：批次内的变化看不见，
/// 是换速度的代价，不是 bug（见 `ResolvedVault` 的「缓存只能是单批次的」）。
#[test]
fn parent_cache_key_is_normalized() {
    let vault = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    fs::create_dir_all(vault.path().join("a")).unwrap();
    fs::write(vault.path().join("a").join("b.md"), "B").unwrap();

    let handle = ResolvedVault::open(vault.path()).unwrap();
    handle.resolve("a/b.md").expect("首次解析应成功");

    // 把 `a` 换成指向库外的 junction：第一道闸若重跑就会拒绝。
    fs::remove_dir_all(vault.path().join("a")).unwrap();
    skip_unless!(
        try_link_dir(&vault.path().join("a"), outside.path()),
        "目录链接"
    );

    handle
        .resolve("./a/b.md")
        .expect("`./a` 与 `a` 是同一个父目录，必须命中同一格缓存");
}

/// 库根必须**规范化一次**：带 `..` 的写法与真实路径是同一个库。
///
/// 不规范化的话 `self.root` 里留着 `..`，而父目录是 `canonicalize` 过的
/// （没有 `..`），`starts_with` 的**组件比较**会失败。症状不是「路径找不到」，
/// 而是**所有笔记都 `PathEscape`**——那是最难往回追的一类错误。
#[test]
fn dotted_root_is_normalized_once() {
    let base = TempDir::new().unwrap();
    let vault = base.path().join("vault");
    fs::create_dir_all(vault.join("a")).unwrap();
    fs::write(vault.join("a").join("b.md"), "B").unwrap();

    let dotted = vault.join("..").join("vault");
    let handle = ResolvedVault::open(&dotted).unwrap();
    let resolved = handle
        .resolve("a/b.md")
        .expect("带 `..` 的库根必须仍能解析");
    assert_eq!(fs::read_to_string(resolved).unwrap(), "B");
}

/* ───────────────────── 写路径：最终组件不得是重解析点 ───────────────────── */

#[test]
fn resolve_for_write_allows_new_file() {
    let vault = TempDir::new().unwrap();
    fs::create_dir_all(vault.path().join("notes")).unwrap();

    let target = resolve_for_write(vault.path(), "notes/new.md").expect("新建笔记应当允许");
    assert!(!target.exists(), "尚未创建，不应存在");
    assert!(target.ends_with(Path::new("notes").join("new.md")));
}

#[test]
fn resolve_for_write_requires_existing_parent() {
    let vault = TempDir::new().unwrap();
    let err = resolve_for_write(vault.path(), "no/such/dir/a.md")
        .expect_err("父目录不存在应当报错而不是凭空造目录");
    assert!(
        format!("{err:?}").contains("Io"),
        "应为 IO 错误，实际 {err:?}"
    );
}

#[test]
fn resolve_for_write_rejects_directory_target() {
    let vault = TempDir::new().unwrap();
    fs::create_dir_all(vault.path().join("sub")).unwrap();

    let err = resolve_for_write(vault.path(), "sub").expect_err("目标是目录本身，不能当文件写");
    // 必须是 NotAFile 而不是 Io：这是「选错对象」，用户该换目标，
    // 不是去查权限或磁盘。混成 Io 会让前端给出完全错误的下一步建议。
    match err {
        ki_core::KiError::NotAFile { path } => assert_eq!(path, "sub"),
        other => panic!("应当是 NotAFile，实际 {other:?}"),
    }
}

/// 缺口 2：`alias.md -> real.md`（都在库内）时，写入必须被拒绝。
///
/// 否则 `note_save("alias.md")` 会静默改写 `real.md`——用户以为在编辑 A，
/// 实际改坏了 B。两个文件都在库里，用户不会怀疑边界，比逃逸更难发现。
#[test]
fn resolve_for_write_rejects_symlink_at_final_component() {
    let vault = TempDir::new().unwrap();
    fs::write(vault.path().join("real.md"), "REAL").unwrap();

    skip_unless!(
        try_link_file(
            &vault.path().join("alias.md"),
            &vault.path().join("real.md")
        ),
        "文件软链"
    );

    let err = resolve_for_write(vault.path(), "alias.md").expect_err("最终组件是软链必须拒绝");
    assert!(format!("{err:?}").contains("PathEscape"), "实际 {err:?}");
    assert_eq!(
        fs::read_to_string(vault.path().join("real.md")).unwrap(),
        "REAL",
        "拒绝之后真实文件必须原封不动"
    );
}

/// 缺口 1 的绕法：只检查父目录是不够的，父目录本身可能就是逃逸软链。
#[test]
fn resolve_for_write_rejects_escape_through_parent_symlink() {
    let vault = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();

    skip_unless!(
        try_link_dir(&vault.path().join("link"), outside.path()),
        "目录链接"
    );

    let err = resolve_for_write(vault.path(), "link/new.md").expect_err("父目录在库外必须拒绝");
    assert!(format!("{err:?}").contains("PathEscape"), "实际 {err:?}");
    assert!(
        !outside.path().join("new.md").exists(),
        "库外目录不该出现任何文件"
    );
}

/// 最终组件是 junction 时同样拒绝——`is_symlink()` 在 Windows 上覆盖 junction。
#[test]
fn resolve_for_write_rejects_junction_at_final_component() {
    let vault = TempDir::new().unwrap();
    fs::create_dir_all(vault.path().join("real_dir")).unwrap();

    skip_unless!(
        try_link_dir(
            &vault.path().join("alias_dir"),
            &vault.path().join("real_dir")
        ),
        "目录链接"
    );

    let err =
        resolve_for_write(vault.path(), "alias_dir").expect_err("最终组件是 junction 必须拒绝");
    assert!(format!("{err:?}").contains("PathEscape"), "实际 {err:?}");
}
