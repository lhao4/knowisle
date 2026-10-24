//! 新建与重命名的磁盘行为。
//!
//! 这一组用例的共同主题是**"什么都没变"**：撞名、非法名、目标已存在——
//! 每条失败路径都要证明磁盘上的旧数据一个字节都没动。数据丢失级的 bug
//! 不会在成功路径上出现，它们都藏在失败分支里。

use std::fs;
use std::path::Path;

use ki_core::KiError;
use ki_vault::{create_note, rename_note};
use tempfile::TempDir;

fn vault() -> TempDir {
    TempDir::new().unwrap()
}

/// 目录里所有条目的名字（不含 `.` / `..`），排序后返回。
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/* ───────────────────── 新建 ───────────────────── */

/// 验收 1 的磁盘侧：文件真的存在、真的是 0 字节、真的在指定目录里。
#[test]
fn create_lands_a_zero_byte_file_in_the_requested_dir() {
    let vault = vault();
    fs::create_dir(vault.path().join("notes")).unwrap();

    let rel = create_note(vault.path(), "notes", "周会").unwrap();

    assert_eq!(rel, "notes/周会.md");
    let absolute = vault.path().join("notes/周会.md");
    assert!(absolute.is_file());
    assert_eq!(fs::metadata(&absolute).unwrap().len(), 0, "必须是 0 字节");
}

/// 根目录新建：`dir` 是空串。
#[test]
fn create_supports_the_vault_root() {
    let vault = vault();
    assert_eq!(
        create_note(vault.path(), "", "根上的笔记").unwrap(),
        "根上的笔记.md"
    );
    assert_eq!(entries(vault.path()), vec!["根上的笔记.md"]);
}

/// 验收 2 的磁盘侧：撞名必须失败，且**原文件的字节与 mtime 都不变**。
#[test]
fn create_never_overwrites_an_existing_note() {
    let vault = vault();
    let target = vault.path().join("a.md");
    fs::write(&target, "原有内容").unwrap();
    let before = fs::metadata(&target).unwrap();

    let err = create_note(vault.path(), "", "a").expect_err("撞名必须失败");

    assert!(matches!(err, KiError::NameTaken { .. }), "实际 {err:?}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "原有内容");
    let after = fs::metadata(&target).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(
        after.modified().unwrap(),
        before.modified().unwrap(),
        "失败路径不该碰目标文件的 mtime"
    );
}

/// 非法名：拒绝，且磁盘上**一个文件都不该多出来**。
///
/// `create_new` 是在校验之后才调的，所以这条同时锁住了"校验发生在落盘之前"。
#[test]
fn create_rejects_bad_names_before_touching_the_disk() {
    let vault = vault();
    for name in ["CON", "a:b", "foo.", "foo ", "", "../x", "周会.ｍｄ"] {
        assert!(
            create_note(vault.path(), "", name).is_err(),
            "{name:?} 应当被拒绝"
        );
    }
    assert!(
        entries(vault.path()).is_empty(),
        "拒绝之后磁盘上不该有任何新文件"
    );
}

/// 父目录不存在时**不代建**。
///
/// 与 `note_save` 的约定一致：目录是用户的结构，程序不替他决定层级。
#[test]
fn create_does_not_invent_directories() {
    let vault = vault();
    assert!(create_note(vault.path(), "不存在的目录", "a").is_err());
    assert!(!vault.path().join("不存在的目录").exists());
}

/* ───────────────────── 重命名 ───────────────────── */

/// 验收 4：只改大小写。
///
/// 在大小写不敏感的文件系统上（Windows / 默认的 macOS APFS），这条会走
/// "目标已存在 → 两步改名"；在 Linux 上目标根本不存在，走普通 `rename`。
/// **同一段代码、同一条断言**，两个平台都必须过。
#[test]
fn case_only_rename_leaves_exactly_one_entry_with_the_new_casing() {
    let vault = vault();
    fs::write(vault.path().join("foo.md"), "内容").unwrap();

    let rel = rename_note(vault.path(), "foo.md", "Foo").unwrap();

    assert_eq!(rel, "Foo.md");
    assert_eq!(entries(vault.path()), vec!["Foo.md"], "目录里只能剩一项");
    assert_eq!(
        fs::read_to_string(vault.path().join("Foo.md")).unwrap(),
        "内容"
    );
}

/// 验收 3 的磁盘侧：目标已存在 → 拒绝，两个文件都不变。
#[test]
fn rename_never_overwrites_an_existing_note() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "A 的内容").unwrap();
    fs::write(vault.path().join("b.md"), "B 的内容").unwrap();

    let err = rename_note(vault.path(), "a.md", "b").expect_err("撞名必须失败");

    assert!(matches!(err, KiError::NameTaken { .. }), "实际 {err:?}");
    assert_eq!(
        fs::read_to_string(vault.path().join("a.md")).unwrap(),
        "A 的内容"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("b.md")).unwrap(),
        "B 的内容"
    );
    assert_eq!(
        entries(vault.path()),
        vec!["a.md", "b.md"],
        "不该有临时文件残留"
    );
}

/// 撞名回滚之后，源文件必须还在**原名**上。
///
/// 两步改名的第一步已经把它挪到临时名了；回滚失败的话用户会看到
/// `.knowisle.rename.*`——内容还在，但名字没了。
#[test]
fn rename_rolls_back_when_the_target_turns_out_to_be_another_file() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "A").unwrap();
    fs::write(vault.path().join("b.md"), "B").unwrap();

    assert!(rename_note(vault.path(), "a.md", "b").is_err());
    assert!(
        vault.path().join("a.md").is_file(),
        "回滚后源文件必须还在原名上"
    );
    assert!(
        entries(vault.path())
            .iter()
            .all(|name| !name.starts_with(".knowisle")),
        "不该留下临时文件：{:?}",
        entries(vault.path())
    );
}

/// 改名保留目录：子目录里的笔记改完还在子目录里。
#[test]
fn rename_stays_in_the_same_directory() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("notes/sub")).unwrap();
    fs::write(vault.path().join("notes/sub/a.md"), "x").unwrap();

    let rel = rename_note(vault.path(), "notes/sub/a.md", "b").unwrap();

    assert_eq!(rel, "notes/sub/b.md");
    assert!(vault.path().join("notes/sub/b.md").is_file());
}

/// 源不存在：报错，不创建任何东西。
#[test]
fn rename_of_a_missing_note_fails_cleanly() {
    let vault = vault();
    assert!(rename_note(vault.path(), "nope.md", "other").is_err());
    assert!(entries(vault.path()).is_empty());
}

/// 改名到非法名：拒绝，原文件不动。
#[test]
fn rename_rejects_bad_names_and_keeps_the_original() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "内容").unwrap();
    for name in ["CON", "a:b", "foo.", "../x", "周会.ｍｄ"] {
        assert!(rename_note(vault.path(), "a.md", name).is_err(), "{name:?}");
    }
    assert_eq!(
        fs::read_to_string(vault.path().join("a.md")).unwrap(),
        "内容"
    );
    assert_eq!(entries(vault.path()), vec!["a.md"]);
}

/// 同名改名是空操作：不搬目录项，也不报错。
#[test]
fn rename_to_the_same_name_is_a_no_op() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "内容").unwrap();
    let before = fs::metadata(vault.path().join("a.md")).unwrap();

    assert_eq!(rename_note(vault.path(), "a.md", "a.md").unwrap(), "a.md");

    let after = fs::metadata(vault.path().join("a.md")).unwrap();
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
}

/// 目录不能被当成笔记改名：`resolve_in_vault` 放行，`rename_note` 必须拒。
///
/// 不拒的话 `fs::rename` 会把整个目录搬走——树里那一片笔记会凭空消失。
#[test]
fn rename_refuses_to_move_a_directory() {
    let vault = vault();
    fs::create_dir(vault.path().join("folder")).unwrap();
    fs::write(vault.path().join("folder/a.md"), "x").unwrap();

    assert!(rename_note(vault.path(), "folder", "moved").is_err());
    assert!(vault.path().join("folder/a.md").is_file());
    assert!(!vault.path().join("moved").exists());
}
