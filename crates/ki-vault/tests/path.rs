//! `path` 模块的安全边界测试：软链逃逸、读写边界。
//!
//! 平台差异与链接夹具见 `tests/common/mod.rs`。

mod common;

use std::fs;
use std::path::Path;

use common::{skip_unless, try_link_dir, try_link_file};
use ki_vault::{resolve_for_write, resolve_in_vault};
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

/// 库内软链**不是**漏洞：解析后仍在库内，按真实路径一样读得到。
///
/// 这条用例的作用是防止将来有人"为了安全"把软链一律禁掉——
/// 那属于策略变更，需要单独讨论，不该顺手做。
#[test]
fn symlink_to_inside_is_allowed() {
    let vault = TempDir::new().unwrap();
    fs::create_dir_all(vault.path().join("sub")).unwrap();
    fs::write(vault.path().join("sub").join("ok.md"), "INSIDE").unwrap();

    skip_unless!(
        try_link_dir(&vault.path().join("link"), &vault.path().join("sub")),
        "目录链接"
    );

    let resolved = resolve_in_vault(vault.path(), "link/ok.md").expect("库内链接应当放行");
    assert_eq!(fs::read_to_string(&resolved).unwrap(), "INSIDE");
    assert!(
        resolved.ends_with(Path::new("sub").join("ok.md")),
        "解析结果必须落到真实路径，实际 {resolved:?}"
    );
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
    assert!(format!("{err:?}").contains("Io"), "实际 {err:?}");
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
