//! 删除的磁盘行为：软删除、回收目录、以及三条拒绝闸。
//!
//! 这一组用例的共同主题是**"文件还在"**。删除是整个 M2 里唯一会毁数据的
//! 操作，所以每条断言都要能回答一个问题：出错的路径上，用户的内容还在不在？
//!
//! 三条闸（目录 / `.knowisle/` / 越界）各自对应一次真实的数据事故，
//! 见 `fileops::delete_note` 的文档。

use std::fs;
use std::path::Path;

use ki_core::KiError;
use ki_vault::{create_note, delete_note, rename_note, scan_vault, ScanOptions, TRASH_DIR};
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

/// 回收目录里所有条目的名字，排序后返回。
///
/// 目录不存在算"空"：被拒绝的删除根本不会建出 `.knowisle/`，
/// 而"没有回收目录"与"回收目录是空的"对断言是同一件事。
fn trash_entries(vault: &TempDir) -> Vec<String> {
    let dir = vault.path().join(TRASH_DIR);
    if !dir.is_dir() {
        return Vec::new();
    }
    entries(&dir)
}

/* ───────────────────── 软删除 ───────────────────── */

/// 验收 1 的磁盘侧：原路径消失、trash 里出现一份**内容完全一致**的副本。
///
/// 判据是字节相等而不是"存在"：软删除最隐蔽的失败是"搬过去了，但搬的是
/// 旧版本"（drain 顺序反了就会出现），那种失败在"文件存在"的断言下全绿。
#[test]
fn delete_moves_the_note_into_trash_and_keeps_every_byte() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "# 标题\n正文一行\n").unwrap();

    let trashed = delete_note(vault.path(), "a.md").unwrap();

    assert!(!vault.path().join("a.md").exists(), "原路径必须消失");
    assert_eq!(
        fs::read_to_string(vault.path().join(&trashed)).unwrap(),
        "# 标题\n正文一行\n",
        "trash 里那份必须与删除前逐字节相同"
    );
}

/// 返回值是**POSIX 相对路径**，且它真的指向刚写下的那个文件。
///
/// 前端与 CLI 都拿这个返回值做提示，所以它不能是"猜出来的名字"。
/// 反斜杠要显式断言：Windows 上只要漏一个 `\` 漏到前端，这个字符串作为
/// Map key 就会与扫描器给出的路径分裂成两份。
#[test]
fn delete_returns_the_posix_trash_path_it_actually_wrote() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "x").unwrap();

    let trashed = delete_note(vault.path(), "a.md").unwrap();

    assert!(
        trashed.starts_with(&format!("{TRASH_DIR}/")),
        "实际 {trashed}"
    );
    assert!(!trashed.contains('\\'), "必须是 POSIX 分隔符：{trashed}");
    assert!(vault.path().join(&trashed).is_file());
}

/// 目标名是 `<unix_ms>-<原名>`：时间戳在前，原名在后。
///
/// 时间戳不是装饰：两次删除靠它分开，否则第二次会把第一次那份覆盖掉
/// ——而"回收目录里互相覆盖"意味着用户以为有两份备份，实际只有一份。
#[test]
fn the_trash_name_is_a_millisecond_stamp_followed_by_the_original_name() {
    let vault = vault();
    fs::write(vault.path().join("周会.md"), "x").unwrap();

    let trashed = delete_note(vault.path(), "周会.md").unwrap();
    let name = trashed.rsplit('/').next().unwrap();

    let (stamp, original) = name.split_once('-').expect("必须是 <时间戳>-<原名>");
    assert!(
        stamp.parse::<u64>().is_ok(),
        "前半段必须是纯数字时间戳：{name}"
    );
    assert_eq!(original, "周会.md", "原名必须原样保留：{name}");
}

/// 验收 7 的磁盘侧：删 `a.md` → 新建 `a.md` → 再删，两份都要在。
#[test]
fn deleting_a_recreated_note_keeps_both_copies() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "第一版").unwrap();
    let first = delete_note(vault.path(), "a.md").unwrap();

    create_note(vault.path(), "", "a").unwrap();
    fs::write(vault.path().join("a.md"), "第二版").unwrap();
    let second = delete_note(vault.path(), "a.md").unwrap();

    assert_ne!(first, second, "两次删除不能落在同一个名字上");
    assert_eq!(trash_entries(&vault).len(), 2, "回收目录里必须有两份");
    assert_eq!(
        fs::read_to_string(vault.path().join(&first)).unwrap(),
        "第一版"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join(&second)).unwrap(),
        "第二版"
    );
}

/// 子目录里的笔记：trash 名里**不许带路径分隔符**。
///
/// `fs::rename` 会把 `notes/sub/a.md` 连同相对路径一起搬到 trash 下
/// （`notes/sub` 不存在时直接失败）。只取 `file_name()` 才是对的行为：
/// 回收目录是平的，它不负责保留原目录结构。
#[test]
fn delete_flattens_the_path_into_a_bare_file_name() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("notes/sub")).unwrap();
    fs::write(vault.path().join("notes/sub/a.md"), "深层").unwrap();

    let trashed = delete_note(vault.path(), "notes/sub/a.md").unwrap();

    let name = trashed.rsplit('/').next().unwrap();
    assert_eq!(name.split_once('-').unwrap().1, "a.md", "实际 {name}");
    assert_eq!(
        fs::read_to_string(vault.path().join(&trashed)).unwrap(),
        "深层"
    );
    assert!(vault.path().join("notes/sub").is_dir(), "目录本身不动");
}

/* ───────────────────── 三条闸 ───────────────────── */

/// 闸 1：目录不能被当成笔记删掉。
///
/// `fs::rename` 对目录完全合法，一次就能把一整片笔记搬进 trash——
/// 树里那一片会凭空消失，而"删除一篇笔记"这个动作看起来成功了。
#[test]
fn delete_refuses_a_directory() {
    let vault = vault();
    fs::create_dir(vault.path().join("folder")).unwrap();
    fs::write(vault.path().join("folder/a.md"), "x").unwrap();

    assert!(delete_note(vault.path(), "folder").is_err());

    assert!(
        vault.path().join("folder/a.md").is_file(),
        "目录里那篇必须还在"
    );
    assert!(trash_entries(&vault).is_empty(), "trash 里不该多出任何东西");
}

/// 闸 2：`.knowisle/` 下的一切都不许删，且报的是**可读**错误。
///
/// 两个具体伤害：删 `index.sqlite` 会让搜索坏掉；删 `trash/x.md` 会把
/// "文件还在"这条唯一的承诺也一起删掉。
#[test]
fn delete_refuses_anything_under_the_metadata_dir() {
    let vault = vault();
    fs::create_dir_all(vault.path().join(TRASH_DIR)).unwrap();
    fs::write(vault.path().join(TRASH_DIR).join("x.md"), "已删过一次").unwrap();
    fs::write(vault.path().join(".knowisle/index.sqlite"), "索引").unwrap();

    for path in [
        ".knowisle/trash/x.md",
        ".knowisle/index.sqlite",
        // 大小写变体：扫描器用 `eq_ignore_ascii_case` 忽略它，
        // 这里若区分大小写，就会出现"树里看不见、却删得掉"的裂缝。
        ".KnowIsle/index.sqlite",
        // 反斜杠变体：Windows 上 `Path::components` 会把它当分隔符。
        ".knowisle\\index.sqlite",
    ] {
        let err = delete_note(vault.path(), path).expect_err("必须拒绝");
        match err {
            KiError::Io { message, .. } => assert!(
                message.contains(".knowisle"),
                "{path} 的错误必须说清是元数据目录：{message}"
            ),
            other => panic!("{path} 报的是 {other:?}"),
        }
    }

    assert_eq!(
        fs::read_to_string(vault.path().join(TRASH_DIR).join("x.md")).unwrap(),
        "已删过一次",
        "回收目录里的那份必须原封不动"
    );
}

/// 闸 3：`..` 是 `PATH_ESCAPE`，不是 IO 错误。
///
/// 验收 10 要的就是这个错误码——前端按 `code` 分支，不解析文案。
#[test]
fn delete_rejects_parent_dir_escape() {
    let vault = vault();
    let err = delete_note(vault.path(), "../x.md").expect_err("必须拒绝");
    assert!(matches!(err, KiError::PathEscape { .. }), "实际 {err:?}");
}

/// 源不存在：报错，且**不留下空壳回收目录**。
///
/// 闸全在 `create_dir_all` 之前，所以一次失败的删除在磁盘上是零副作用。
#[test]
fn delete_of_a_missing_note_fails_and_creates_nothing() {
    let vault = vault();
    assert!(delete_note(vault.path(), "nope.md").is_err());
    assert!(
        !vault.path().join(".knowisle").exists(),
        "不该凭空造出 .knowisle"
    );
    assert!(entries(vault.path()).is_empty());
}

/* ───────────────────── 与扫描器的关系 ───────────────────── */

/// 删掉的东西从树里消失，**且不需要任何额外过滤**。
///
/// 这是决定 (a)③ 的可执行形式：`.knowisle` 在 `IGNORED_DIRS` 里，所以
/// trash 目录根本不会被下降。若哪天有人把 `.knowisle` 从忽略名单里拿掉，
/// 用户会看到回收目录里的文件名出现在侧栏——这条会先红。
#[test]
fn trashed_notes_disappear_from_the_scan() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "x").unwrap();
    fs::write(vault.path().join("b.md"), "y").unwrap();

    delete_note(vault.path(), "a.md").unwrap();

    let result = scan_vault(vault.path(), &ScanOptions::default()).unwrap();
    assert_eq!(result.total_files, 1, "只剩 b.md");
    assert_eq!(result.total_dirs, 0, "回收目录不该出现在树里");
    let names: Vec<&str> = result
        .tree
        .children
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|node| node.name.as_str())
        .collect();
    assert_eq!(names, vec!["b.md"]);
}

/* ───────────────────── 改名补上的同一条闸 ───────────────────── */

/// `.knowisle/` 的闸也补给了改名：改回收目录里的文件名同样不该被允许。
///
/// 这条在 M2 第五刀之前是漏的——`note_rename(".knowisle/trash/x.md", "y")`
/// 能成功，于是"已删除"的那份被改了名，用户去翻文件夹时对不上。
#[test]
fn rename_refuses_metadata_paths_too() {
    let vault = vault();
    fs::create_dir_all(vault.path().join(TRASH_DIR)).unwrap();
    fs::write(vault.path().join(TRASH_DIR).join("x.md"), "x").unwrap();

    let err = rename_note(vault.path(), ".knowisle/trash/x.md", "y").expect_err("必须拒绝");
    assert!(matches!(err, KiError::Io { .. }), "实际 {err:?}");
    assert_eq!(
        trash_entries(&vault),
        vec!["x.md"],
        "回收目录里的名字不该变"
    );
}
