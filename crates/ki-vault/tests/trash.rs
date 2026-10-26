//! 回收站的磁盘行为：清单、恢复、撞名、降级、清空。
//!
//! 这一组用例的共同主题是**"删掉的东西回得来"**。删除本身在 `tests/delete.rs`
//! 里验过（文件进了回收目录、一个字节没少）；这里验的是另一半：**能不能回到
//! 原来的目录**，以及"回不去的时候，用户看到什么"。
//!
//! # 三条降级路径必须都有用例
//!
//! 清单是磁盘上的普通文件，它可以丢、可以坏、可以被人改。三条降级各自对应
//! 一次真实的失败，而它们的**共同点**是：文件仍然回得来，只是落点变了。
//! 如果哪一天某个改动让"清单坏了"变成"文件没了"，这里会先红。
//!
//! 但降级的原因必须分开报：**没有记录**（`recordMissing`）、**记录坏了**
//! （`recordCorrupt`）、**原目录没了**（`originGone`）是三个不同的下一步。
//! 压成一个布尔时，第三种会被说成"原路径记录缺失"——记录没丢，是目录没了。
//!
//! # 不降级的那些失败也必须有用例
//!
//! 只读父目录、父目录链不可进入、路径上有链接，都不该把文件悄悄搬到库根。
//! 这三条是「降级判据收窄」的守卫：判据一旦放宽回"任何错误都降级"，
//! 它们会先红（见 ADR-008 的变异验证）。
//!
//! # 原子性怎么验
//!
//! `kill -9` **证明不了**原子写：进程死了，页缓存还在，`fs::write` 写下去
//! 的字节一样会在重启后读到。真正的判据是「读者永远看不到半个文件」——
//! 见 [`manifest_records_are_never_observed_half_written`]，那是把
//! `atomic_replace` 换成 `fs::write` 之后唯一会变红的用例。

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{skip_unless, try_link_dir};
use ki_core::{Fallback, KiError};
// 只在 Unix 的权限用例里用到：Windows 上那两条是 `#[cfg(unix)]`。
#[cfg(unix)]
use ki_core::IoKind;
use ki_vault::{
    delete_note, read_note, trash_page, trash_purge, trash_restore, MANIFEST_DIR, TRASH_DIR,
};
use tempfile::TempDir;

fn vault() -> TempDir {
    TempDir::new().unwrap()
}

/// 把所有页翻完，拼成一个列表。
///
/// 这些用例的条目都是个位数，分页对它们是透明的；`page` 返回 `next` 而不是
/// 靠 `len < limit` 推断，所以这个 helper 不会在最后一页多转一圈。
fn trash_list(root: &Path) -> Result<Vec<ki_core::TrashEntry>, KiError> {
    let mut all = Vec::new();
    let mut cursor = None;
    loop {
        let page = trash_page(root, cursor.as_ref(), 50)?;
        all.extend(page.entries);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(all),
        }
    }
}

/// 回收目录里的条目名（排序后）。
fn trash_entries(vault: &TempDir) -> Vec<String> {
    let dir = vault.path().join(TRASH_DIR);
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// 清单目录里的记录文件名（排序后）。
fn manifest_entries(root: &Path) -> Vec<String> {
    let dir = root.join(MANIFEST_DIR);
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    names
}

/* ───────────────────── 删除时记住原路径 ───────────────────── */

/// 验收 1：删 `a/b/c.md` → 列表一条，原路径就是 `a/b/c.md`。
///
/// 这条是整个 M5 的存在理由。文件名里只有 `c.md`，目录丢了就再也回不去——
/// 而"回到原来的目录"是用户对"恢复"的全部期待。
#[test]
fn delete_records_the_original_path_of_a_nested_note() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    fs::write(vault.path().join("a/b/c.md"), "# 正文\n").unwrap();

    let trashed = delete_note(vault.path(), "a/b/c.md").unwrap();
    let listed = trash_list(vault.path()).unwrap();

    assert_eq!(listed.len(), 1);
    let first = &listed[0];
    assert_eq!(first.entry, trashed.rsplit('/').next().unwrap());
    assert_eq!(first.name, "c.md", "展示名是原名的末段");
    assert_eq!(first.origin.as_deref(), Some("a/b/c.md"));
    assert_eq!(first.size, "# 正文\n".len() as u64);
    // 时间戳必须落在"刚刚"附近：它决定列表顺序，错一天就会把旧删除排到最前。
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        (now - first.deleted_at).abs() < 60_000,
        "deletedAt 应当是当前时间，实际 {}（现在 {now}）",
        first.deleted_at
    );
}

/// 一条记录一个文件：删两次 → 清单里两条，互不覆盖。
///
/// 单文件清单在这里会退化成"读全量 → 改 → 写全量"，而那条路径的每一次
/// 崩溃都可能把**别人的**原路径一起写丢。
#[test]
fn every_delete_appends_its_own_manifest_record() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "A").unwrap();
    fs::write(vault.path().join("b.md"), "B").unwrap();

    delete_note(vault.path(), "a.md").unwrap();
    delete_note(vault.path(), "b.md").unwrap();

    assert_eq!(manifest_entries(vault.path()).len(), 2);
    let origins: Vec<String> = trash_list(vault.path())
        .unwrap()
        .into_iter()
        .filter_map(|entry| entry.origin)
        .collect();
    assert_eq!(origins.len(), 2, "两条都要有原路径");
    assert!(origins.contains(&"a.md".to_string()));
    assert!(origins.contains(&"b.md".to_string()));
}

/// 没有回收目录时列表是空的，不是错误。
///
/// 判据很重要：从没删过东西的库不该在打开回收站时看到一个报错。
#[test]
fn listing_a_vault_that_never_deleted_anything_is_empty() {
    let vault = vault();
    assert!(trash_list(vault.path()).unwrap().is_empty());
    assert_eq!(trash_purge(vault.path()).unwrap().removed, 0);
    assert!(
        !vault.path().join(".knowisle").exists(),
        "只读操作不该凭空造出 .knowisle"
    );
}

/* ───────────────────── 恢复 ───────────────────── */

/// 验收 2：恢复回原目录，内容逐字节一致，清单与回收目录都清空。
///
/// 内容用字节相等而不是"文件存在"：软删除最隐蔽的失败是"搬回去了，但搬的是
/// 旧版本"（drain 顺序反了），那种失败在"存在"的断言下全绿。
#[test]
fn restore_returns_the_note_to_its_original_directory() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    fs::write(vault.path().join("a/b/c.md"), "# 标题\n正文一行\n").unwrap();

    delete_note(vault.path(), "a/b/c.md").unwrap();
    let result = trash_restore(vault.path(), &trash_entries(&vault)[0]).unwrap();

    assert_eq!(result.path, "a/b/c.md");
    assert_eq!(result.requested, "a/b/c.md");
    assert!(!result.renamed);
    assert_eq!(result.fallback, None, "原目录还在，不该有任何降级");
    assert!(
        !result.indexed,
        "ki-vault 不索引：返回时它必须是 false，由命令层补完索引再置真"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("a/b/c.md")).unwrap(),
        "# 标题\n正文一行\n"
    );
    assert!(trash_entries(&vault).is_empty(), "回收目录必须清空");
    assert!(
        manifest_entries(vault.path()).is_empty(),
        "恢复之后记录必须删掉，否则它会变成孤儿"
    );
    // 恢复出来的东西必须还能被读成笔记（扩展名、编码都没被动过）。
    assert_eq!(read_note(vault.path(), "a/b/c.md").unwrap().size, 22);
}

/// 验收 3：撞名**不覆盖**，改名为 `c 2.md`，两份都在。
///
/// 覆盖是这条路径上唯一不可逆的错误：用户删了旧版、又写了新版，恢复时若
/// 直接覆盖，新版就没了——而新版是他**唯一还想要的那份**。
#[test]
fn restore_never_overwrites_an_existing_note() {
    let vault = vault();
    fs::write(vault.path().join("c.md"), "删掉的那份").unwrap();
    delete_note(vault.path(), "c.md").unwrap();
    fs::write(vault.path().join("c.md"), "新写的那份").unwrap();

    let result = trash_restore(vault.path(), &trash_entries(&vault)[0]).unwrap();

    assert!(result.renamed, "撞名必须报告 renamed");
    assert_eq!(result.requested, "c.md");
    assert_eq!(result.path, "c 2.md");
    assert_eq!(
        fs::read_to_string(vault.path().join("c.md")).unwrap(),
        "新写的那份",
        "已存在的那份一个字节都不许动"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("c 2.md")).unwrap(),
        "删掉的那份"
    );
}

/// 连续撞名：`c.md`、`c 2.md` 都在时落到 `c 3.md`。
#[test]
fn consecutive_collisions_keep_counting_up() {
    let vault = vault();
    fs::write(vault.path().join("c.md"), "第一版").unwrap();
    delete_note(vault.path(), "c.md").unwrap();
    fs::write(vault.path().join("c.md"), "第二版").unwrap();
    delete_note(vault.path(), "c.md").unwrap();
    fs::write(vault.path().join("c.md"), "现在的").unwrap();
    fs::write(vault.path().join("c 2.md"), "占位").unwrap();

    let mut paths: Vec<String> = Vec::new();
    while let Some(name) = trash_entries(&vault).into_iter().next() {
        paths.push(trash_restore(vault.path(), &name).unwrap().path);
    }
    paths.sort();

    assert_eq!(paths, vec!["c 3.md", "c 4.md"]);
    assert_eq!(
        fs::read_to_string(vault.path().join("c.md")).unwrap(),
        "现在的"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("c 2.md")).unwrap(),
        "占位"
    );
}

/* ───────────────────── 降级路径 ───────────────────── */

/// 验收 2：清单写坏 → 列表只显示文件名，恢复落到库根目录并报 `recordCorrupt`。
///
/// "写坏"用的是最真实的一种：文件在，但内容不是合法 JSON（同步工具截断、
/// 磁盘坏块、用户手改）。整条记录作废，但**文件本身仍然可用**。
///
/// 这里的断言重点是 `RecordCorrupt` **不是** `RecordMissing`：记录就在那儿，
/// 只是读不出来。两种事实对用户是两句话（"没有这条记录" vs "记录损坏"）。
#[test]
fn a_corrupt_manifest_degrades_to_the_file_name_only() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    fs::write(vault.path().join("a/b/c.md"), "内容").unwrap();
    delete_note(vault.path(), "a/b/c.md").unwrap();

    let record = vault
        .path()
        .join(MANIFEST_DIR)
        .join(format!("{}.json", trash_entries(&vault)[0]));
    fs::write(&record, "{\"origin\":\"a/b/c.md\",\"deletedAt\":").unwrap();

    let listed = trash_list(vault.path()).unwrap();
    assert_eq!(listed.len(), 1, "记录坏了不代表文件没了");
    assert!(listed[0].origin.is_none(), "损坏的记录不能被当成原路径");
    // 时间戳退到文件名前缀：列表仍然有序，用户仍然找得到它。
    assert!(listed[0].deleted_at > 0);

    let result = trash_restore(vault.path(), &listed[0].entry).unwrap();
    assert_eq!(result.fallback, Some(Fallback::RecordCorrupt));
    assert_eq!(result.path, "c.md", "降级后落在库根目录");
    assert_eq!(result.requested, "c.md", "记录不可用时，意图只能从文件名猜");
    assert!(!result.renamed, "落到库根不是改名：根目录里没有同名文件");
    assert_eq!(
        fs::read_to_string(vault.path().join("c.md")).unwrap(),
        "内容"
    );
}

/// 清单里的原路径被改成越界路径：**不显示、不执行**，按"记录不可用"处理。
///
/// 清单是磁盘上的普通文件，任何人都能改它。`../../.ssh/id_rsa` 若被当成
/// 原路径显示出来，用户会以为自己的笔记原本在那儿；若被拿去恢复，
/// 就是一次跨库搬运。
///
/// 它**不是** `recordMissing`：记录文件好好地在那儿，坏的是里面的内容。
/// 说成"没有这条记录"会让用户去找一个明明存在的文件。
#[test]
fn a_tampered_origin_is_treated_as_an_unusable_record() {
    let vault = vault();
    fs::write(vault.path().join("c.md"), "内容").unwrap();
    delete_note(vault.path(), "c.md").unwrap();

    let record = vault
        .path()
        .join(MANIFEST_DIR)
        .join(format!("{}.json", trash_entries(&vault)[0]));
    fs::write(
        &record,
        r#"{"origin":"../../.ssh/id_rsa","deletedAt":1700000000000}"#,
    )
    .unwrap();

    let listed = trash_list(vault.path()).unwrap();
    assert!(listed[0].origin.is_none(), "越界路径不能被显示成原路径");

    let result = trash_restore(vault.path(), &listed[0].entry).unwrap();
    assert_eq!(result.fallback, Some(Fallback::RecordCorrupt));
    assert_eq!(result.path, "c.md");
}

/// 清单根本写不进去（`manifest` 被一个同名文件占了）→ **删除照样成功**。
///
/// 这是"先搬文件、后写清单"的直接后果：文件已经安全了，这时让删除报错只会
/// 引导用户再删一次，而第二次必然报"文件不存在"。
#[test]
fn a_delete_succeeds_even_when_the_manifest_cannot_be_written() {
    let vault = vault();
    fs::create_dir_all(vault.path().join(TRASH_DIR)).unwrap();
    // 用一个普通文件占住清单目录的位置：`create_dir_all` 会失败。
    fs::write(vault.path().join(MANIFEST_DIR), "占位").unwrap();
    fs::write(vault.path().join("a.md"), "内容").unwrap();

    let trashed = delete_note(vault.path(), "a.md").expect("清单写不了也必须删成功");

    assert_eq!(
        fs::read_to_string(vault.path().join(&trashed)).unwrap(),
        "内容"
    );
    let listed = trash_list(vault.path()).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].origin.is_none(), "没有清单就只能显示文件名");
    let restored = trash_restore(vault.path(), &listed[0].entry).unwrap();
    assert_eq!(restored.path, "a.md");
    // 清单**目录**被一个同名文件占了：记录读不出来。具体归到哪一种取决于
    // 操作系统的错误码（Windows 报 ERROR_PATH_NOT_FOUND → `recordMissing`，
    // Unix 报 ENOTDIR → `recordCorrupt`），**但绝不能是 `originGone`**：
    // 那会说"原目录没了"，而原目录就在那儿。
    assert!(
        matches!(
            restored.fallback,
            Some(Fallback::RecordMissing | Fallback::RecordCorrupt)
        ),
        "实际 {:?}",
        restored.fallback
    );
}

/// 崩溃在"文件已搬走、清单还没写"之间：条目仍在，恢复落到根目录。
///
/// 这正是删除的两个步骤之间那个窗口。造法就是删掉记录文件本身——
/// 与"崩溃时记录还没落盘"在磁盘上是同一个状态。
///
/// 它是 `recordMissing` 的**唯一**正例：记录文件真的不在。
#[test]
fn an_entry_without_a_record_is_still_listed_and_restorable() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    fs::write(vault.path().join("a/b/c.md"), "内容").unwrap();
    delete_note(vault.path(), "a/b/c.md").unwrap();
    fs::remove_file(
        vault
            .path()
            .join(MANIFEST_DIR)
            .join(format!("{}.json", trash_entries(&vault)[0])),
    )
    .unwrap();

    let listed = trash_list(vault.path()).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].origin.is_none());

    let result = trash_restore(vault.path(), &listed[0].entry).unwrap();
    assert_eq!(result.fallback, Some(Fallback::RecordMissing));
    assert_eq!(result.path, "c.md", "从文件名里剥出原名，落在库根");
    assert_eq!(result.requested, "c.md", "没有记录时，意图只能从文件名猜");
    assert!(!result.renamed);
    assert!(vault.path().join("c.md").is_file());
}

/// 原目录在外部被删掉了 → 不重建目录，落到库根并报 `originGone`。
///
/// "悄悄重建一串目录"比"落到根目录并明确告知"更容易让用户困惑：
/// 他会以为那个目录一直都在。
///
/// # 这条用例钉住 `requested` / `path` 的分工
///
/// `requested` 是**意图**：`gone/c.md`——他当时就是从那儿删的，记录没丢。
/// `path` 是**事实**：`c.md`。两者不同，但**不是改名**（`renamed` 必须为假）。
/// 用 `path != requested` 当改名判据的实现会在这里红。
#[test]
fn restore_falls_back_to_the_root_when_the_original_directory_is_gone() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("gone")).unwrap();
    fs::write(vault.path().join("gone/c.md"), "内容").unwrap();
    delete_note(vault.path(), "gone/c.md").unwrap();
    fs::remove_dir_all(vault.path().join("gone")).unwrap();

    let result = trash_restore(vault.path(), &trash_entries(&vault)[0]).unwrap();

    assert_eq!(result.fallback, Some(Fallback::OriginGone));
    assert_eq!(result.path, "c.md");
    assert_eq!(result.requested, "gone/c.md", "意图仍然是原路径：记录没丢");
    assert!(!result.renamed, "落到库根不是改名：根目录里没有同名文件");
    assert!(
        !vault.path().join("gone").exists(),
        "不代建原目录：那是外部删除，不是我们的决定"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("c.md")).unwrap(),
        "内容"
    );
}

/// 原路径上有一个**同名目录** → 不是降级，在原目录里改名成 `c 2.md`。
///
/// 这是"降级判据收窄"最容易被写错的一条：旧实现用
/// `resolve_for_write(...).ok()` 判"原目录可不可用"，而那个函数**顺带**检查
/// 了最终组件是不是目录（`NotAFile`）。于是"名字被占"被误判成"原目录没了"，
/// 文件被悄悄搬到库根——用户在原目录里永远找不到它。
///
/// 判据拆开之后：父目录可用（`ensure_parent_dir_usable`），名字被占
/// （`occupied` + `unique_target`）→ 改名，`fallback` 为 `null`。
#[test]
fn a_directory_at_the_original_path_is_not_a_reason_to_fall_back() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    fs::write(vault.path().join("a/b/c.md"), "内容").unwrap();
    delete_note(vault.path(), "a/b/c.md").unwrap();

    // 原路径上放一个同名**目录**。`occupied` 用的是 `symlink_metadata`，
    // 目录同样算"这个名字有人占"。
    fs::create_dir_all(vault.path().join("a/b/c.md")).unwrap();

    let result = trash_restore(vault.path(), &trash_entries(&vault)[0]).unwrap();

    assert_eq!(result.fallback, None, "原目录好好的，没有任何降级");
    assert!(result.renamed, "撞名必须报告 renamed");
    assert_eq!(result.requested, "a/b/c.md");
    assert_eq!(result.path, "a/b/c 2.md");
    assert!(
        !vault.path().join("c.md").exists(),
        "绝不许落到库根：用户要的是原目录里的另一个名字"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("a/b/c 2.md")).unwrap(),
        "内容"
    );
    assert!(
        vault.path().join("a/b/c.md").is_dir(),
        "占住名字的那个目录一个字节都不许动"
    );
}

/* ───────────────────── 不降级的失败：必须冒泡 ───────────────────── */

/// 改权限（Unix）。测试跑完必须改回来，否则 `TempDir` 连自己都删不掉。
#[cfg(unix)]
fn chmod(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
}

/// 验收 5：原目录**只读** → 命令报 `IO`，文件留在回收站，**不落库根**。
///
/// # 为什么必须设父目录 555，而不是把文件设成只读
///
/// Unix 上 `rename` 覆盖一个只读**文件**是完全合法的——权限判在目录上。
/// 把文件设成 444 再断言"恢复失败"会得到一个永远绿的假用例。
///
/// # 这条用例守的是什么
///
/// 用户点恢复，是因为他以为文件会回到原处。只读时我们搬不动，此时**报错**
/// 比"悄悄放到库根"好：前者他知道去改权限，后者他会在原目录里找一个
/// 根本不存在的文件。条目留在回收站里，改完权限再点一次就行。
#[cfg(unix)]
#[test]
fn restore_reports_io_when_the_original_directory_is_read_only() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    fs::write(vault.path().join("a/b/c.md"), "内容").unwrap();
    delete_note(vault.path(), "a/b/c.md").unwrap();

    chmod(&vault.path().join("a/b"), 0o555);
    let entry = trash_entries(&vault)[0].clone();
    let result = trash_restore(vault.path(), &entry);
    // 先恢复权限再断言：`assert!` 失败会 panic，而 TempDir 清不掉 555 的目录。
    chmod(&vault.path().join("a/b"), 0o755);

    let err = result.expect_err("只读目录里搬不进文件，必须报错");
    match err {
        KiError::Io { kind, .. } => {
            assert_eq!(kind, IoKind::PermissionDenied, "实际 {kind:?}")
        }
        other => panic!("应当是 IO/PermissionDenied，实际 {other:?}"),
    }
    assert_eq!(
        trash_entries(&vault),
        vec![entry],
        "失败时条目必须还在回收站"
    );
    assert!(
        !vault.path().join("c.md").exists(),
        "绝不许把搬不动的东西悄悄放到库根"
    );
}

/// 父目录链**不可进入**（`chmod 000`）→ 报 `IO`，不降级。
///
/// # 为什么这条比 555 那条更关键
///
/// 555 的失败发生在 `rename` 那一刻（`ensure_parent_dir_usable` 只做
/// `canonicalize`，读权限够用），所以它**验不出降级判据**——旧实现与
/// "任何错误都降级"的变异实现在这条上都是绿的。
///
/// `chmod 000` 让 `canonicalize(<root>/a/b)` 直接 `EACCES`（解析 `b` 需要
/// `a` 的可进入位），于是错误**发生在判据那一步**：只有 `NotFound` 才降级的
/// 实现会把它冒泡出去，而"任何错误都降级"的实现会把文件搬到库根。
///
/// 跳过条件：以 root 运行时权限位不生效，这条用例什么都证明不了。
#[cfg(unix)]
#[test]
fn restore_never_falls_back_when_the_original_path_is_unreachable() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    fs::write(vault.path().join("a/b/c.md"), "内容").unwrap();
    delete_note(vault.path(), "a/b/c.md").unwrap();

    chmod(&vault.path().join("a"), 0o000);
    // root 下 000 也读得进去：用例会变成"恢复成功"，而那不是实现的问题。
    let enforced = fs::metadata(vault.path().join("a/b")).is_err();
    if !enforced {
        chmod(&vault.path().join("a"), 0o755);
        eprintln!("跳过：当前用户不受权限位约束（多半是 root），这条用例证明不了降级判据");
        return;
    }

    let entry = trash_entries(&vault)[0].clone();
    let result = trash_restore(vault.path(), &entry);
    chmod(&vault.path().join("a"), 0o755);

    let err = result.expect_err("路径不可进入时必须报错，不能替用户换个地方");
    match err {
        KiError::Io { kind, .. } => {
            assert_eq!(kind, IoKind::PermissionDenied, "实际 {kind:?}")
        }
        other => panic!("应当是 IO/PermissionDenied，实际 {other:?}"),
    }
    assert_eq!(
        trash_entries(&vault),
        vec![entry],
        "失败时条目必须还在回收站"
    );
    assert!(
        !vault.path().join("c.md").exists(),
        "绝不许把文件悄悄搬到库根"
    );
}

/// 原路径的父目录变成了一个**链接** → 报 `PATH_ESCAPE`，不降级。
///
/// 扫描器不进入重解析点，写路径也一律拒绝（`path` 模块的模块文档）。
/// 恢复走的是同一条边界：既然"那个目录"现在指向别处，把它当成原目录用
/// 就等于绕过所有既有闸门。宁可报错让用户自己看一眼，也不搬。
///
/// 这条在 Windows 上也能跑（junction 不需要管理员权限），是"只有
/// `NotFound` 才降级"在 Windows 上唯一的可执行判据——Unix 那两条靠权限位。
#[test]
fn restore_refuses_a_path_that_now_goes_through_a_link() {
    let vault = vault();
    let outside = TempDir::new().unwrap();
    fs::create_dir_all(vault.path().join("a/b")).unwrap();
    fs::write(vault.path().join("a/b/c.md"), "内容").unwrap();
    delete_note(vault.path(), "a/b/c.md").unwrap();
    fs::remove_dir_all(vault.path().join("a/b")).unwrap();

    skip_unless!(
        try_link_dir(&vault.path().join("a/b"), outside.path()),
        "目录链接"
    );

    let entry = trash_entries(&vault)[0].clone();
    let err = trash_restore(vault.path(), &entry)
        .expect_err("原目录变成链接时必须报错，不能替用户换个地方");
    assert!(
        matches!(err, KiError::PathEscape { .. }),
        "应当是 PATH_ESCAPE，实际 {err:?}"
    );
    assert_eq!(
        trash_entries(&vault),
        vec![entry],
        "失败时条目必须还在回收站"
    );
    assert!(
        !vault.path().join("c.md").exists(),
        "绝不许把文件悄悄搬到库根"
    );
}

/* ───────────────────── 外部干扰 ───────────────────── */

/// 条目被外部删掉（用户手工清空回收目录）→ 列表里没有它，恢复报 `NOT_FOUND`。
///
/// 报 IO/NotFound 而不是内部错误：用户下一步该做的是**刷新列表**，
/// 而不是"删掉索引让它重建"。
#[test]
fn an_entry_deleted_outside_the_app_disappears_and_restore_fails_cleanly() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "内容").unwrap();
    let trashed = delete_note(vault.path(), "a.md").unwrap();
    let name = trashed.rsplit('/').next().unwrap().to_owned();

    fs::remove_file(vault.path().join(&trashed)).unwrap();

    assert!(trash_list(vault.path()).unwrap().is_empty());

    let err = trash_restore(vault.path(), &name).expect_err("文件没了，恢复必须失败");
    match err {
        KiError::Io { kind, .. } => {
            assert_eq!(kind, ki_core::IoKind::NotFound, "实际 {kind:?}")
        }
        other => panic!("应当是 IO/NotFound，实际 {other:?}"),
    }
}

/// 恢复的入口只接受**单个文件名**：`..`、`/`、`\` 一律 `PATH_ESCAPE`。
///
/// IPC 是公开边界。放行 `../x.md` 就能把库外的文件搬进库，或把回收目录里的
/// 文件搬到库外——两者都不需要任何权限提升。
///
/// # 反斜杠为什么按平台分支
///
/// `\` **只在 Windows 上是分隔符**。Unix 上它是普通字符，`a\b.md` 就是回收
/// 目录里一个合法文件名——`trash_list` 会把它列出来，恢复入口必须能收回去，
/// 否则那个条目永远恢复不出来。
///
/// 这条和 `check_entry` 的单测是同一个坑的两处：上一刀把它们都写成了跨平台
/// 的 bad 列表，Windows 全绿、Linux/macOS 红。
#[test]
fn restore_refuses_entry_names_that_escape_the_trash_directory() {
    let vault = vault();
    fs::create_dir_all(vault.path().join(TRASH_DIR)).unwrap();
    fs::write(vault.path().join(TRASH_DIR).join("x.md"), "x").unwrap();

    for bad in ["../x.md", "..", ".", "a/b.md", "/x.md"] {
        let err = trash_restore(vault.path(), bad).expect_err(bad);
        assert!(
            matches!(err, KiError::PathEscape { .. }),
            "{bad} 应当是 PathEscape，实际 {err:?}"
        );
    }

    // Windows：`\` 是分隔符，`a\b.md` 会指向 `trash/a/b.md`，必须拒。
    #[cfg(windows)]
    {
        let err = trash_restore(vault.path(), "a\\b.md").expect_err("反斜杠在 Windows 上是分隔符");
        assert!(
            matches!(err, KiError::PathEscape { .. }),
            "应当是 PathEscape，实际 {err:?}"
        );
    }

    // Unix：放行到 IO 层（回收目录里没有这个条目），而不是 PathEscape。
    #[cfg(unix)]
    {
        let err = trash_restore(vault.path(), "a\\b.md").expect_err("回收目录里没有这个条目");
        assert!(
            matches!(err, KiError::Io { .. }),
            "Unix 上反斜杠不是分隔符，应当放行到 IO 层，实际 {err:?}"
        );
    }

    assert!(
        vault.path().join(TRASH_DIR).join("x.md").is_file(),
        "被拒绝的请求不该动任何文件"
    );
}

/// 回收目录里出现一个目录：列表跳过它，清空也不递归删它。
///
/// 递归删错一次的代价是不可逆的。回收目录是平的，我们只往里放文件，
/// 所以"这里有目录"只可能是用户自己放的——不在"清空回收站"的承诺里。
#[test]
fn purge_skips_directories_it_did_not_create() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "内容").unwrap();
    delete_note(vault.path(), "a.md").unwrap();
    fs::create_dir_all(vault.path().join(TRASH_DIR).join("mine")).unwrap();
    fs::write(vault.path().join(TRASH_DIR).join("mine/keep.txt"), "x").unwrap();

    assert_eq!(trash_list(vault.path()).unwrap().len(), 1, "目录不算条目");
    assert_eq!(
        trash_page(vault.path(), None, 10).unwrap().total,
        1,
        "目录不出现，也不计入 total"
    );

    let result = trash_purge(vault.path()).unwrap();
    assert_eq!(result.removed, 1, "只数删掉的文件，不数清单目录");
    assert_eq!(result.failed, 0, "跳过的目录不是失败：它本来就不在承诺里");
    assert!(vault.path().join(TRASH_DIR).join("mine/keep.txt").is_file());
    assert!(trash_list(vault.path()).unwrap().is_empty());
}

/* ───────────────────── 清空 ───────────────────── */

/// 验收 6：清空之后回收目录空了、清单空了，再列是空表。
#[test]
fn purge_empties_the_trash_and_the_manifest() {
    let vault = vault();
    for name in ["a.md", "b.md", "c.md"] {
        fs::write(vault.path().join(name), name).unwrap();
        delete_note(vault.path(), name).unwrap();
    }

    let result = trash_purge(vault.path()).unwrap();

    assert_eq!(result.removed, 3);
    assert_eq!(result.failed, 0);
    assert!(trash_entries(&vault).is_empty(), "回收目录必须空");
    assert!(
        !vault.path().join(MANIFEST_DIR).exists(),
        "清单目录也必须清掉，否则下一次删除会读到一堆孤儿记录"
    );
    assert!(trash_list(vault.path()).unwrap().is_empty());
    // 清空之后还能继续用：再删一次仍然有记录。
    fs::write(vault.path().join("d.md"), "d").unwrap();
    delete_note(vault.path(), "d.md").unwrap();
    assert_eq!(
        trash_list(vault.path()).unwrap()[0].origin.as_deref(),
        Some("d.md")
    );
}

/// 验收 7（部分）：一篇删不掉 → **不返回错误**，其余照删，计数如实。
///
/// Windows 用独占句柄占住一个文件（`share_mode(0)`），删它必然共享冲突。
/// 这条钉住的是"部分失败仍然返回 Ok"：旧实现返回第一个 `Err` 并丢掉
/// `removed`，前端拿到错误就 return，面板上于是还列着已经被删掉的文件。
#[cfg(windows)]
#[test]
fn purge_counts_the_file_it_cannot_delete() {
    use std::os::windows::fs::OpenOptionsExt;

    let vault = vault();
    for name in ["a.md", "b.md", "c.md"] {
        fs::write(vault.path().join(name), name).unwrap();
        delete_note(vault.path(), name).unwrap();
    }
    let held = trash_entries(&vault)
        .into_iter()
        .find(|name| name.ends_with("-b.md"))
        .expect("回收目录里应当有 b.md");

    // share_mode(0)：不给别人任何共享权限，`remove_file` 会拿到共享冲突。
    let handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(vault.path().join(TRASH_DIR).join(&held))
        .expect("打开待占用文件");

    let result = trash_purge(vault.path()).expect("删不掉不能返回错误");
    assert_eq!(result.removed, 2, "其余两篇必须照删");
    assert_eq!(result.failed, 1);
    assert_eq!(
        trash_entries(&vault),
        vec![held.clone()],
        "删不掉的那篇仍在回收目录里，列表刷新后还要看得见"
    );

    // 清空时清单**按条清**，不能整块删。整块 `remove_dir_all` 会连带删掉
    // 这一篇的记录：文件还在回收目录里，原路径却没了，列表上那一条当场
    // 退化成「原路径未知」——刚说完"删不掉的已留在列表里"，就把它的原路径
    // 扔了。断言"记录还在"比断言"目录还在"更直接。
    assert_eq!(
        trash_list(vault.path()).unwrap()[0].origin.as_deref(),
        Some("b.md"),
        "删不掉的那篇必须保住原路径，否则刷新后它会变成「原路径未知」"
    );
    assert!(
        vault
            .path()
            .join(MANIFEST_DIR)
            .join(format!("{held}.json"))
            .is_file(),
        "它的清单记录必须还在磁盘上"
    );

    drop(handle);

    // 占住它的句柄放掉之后，再清一次能彻底清干净——按条清的判据是
    // 「文件还在不在」，文件没了记录就该走，清单目录也该跟着消失。
    let again = trash_purge(vault.path()).expect("第二次清空");
    assert_eq!((again.removed, again.failed), (1, 0));
    assert!(
        !vault.path().join(MANIFEST_DIR).exists(),
        "全清干净之后清单目录不能留下空壳"
    );
}

/// 验收 7（另一支）：整个回收目录只读 → 全部删不掉，但仍然 `Ok`。
///
/// Unix 上没法用"独占句柄"占住文件（`unlink` 不管有没有人开着），
/// 所以用只读父目录让 `unlink` 失败。断言的是计数与"不报错"，
/// 而不是"删掉了 N-1 篇"——那一条由 Windows 的用例覆盖。
#[cfg(unix)]
#[test]
fn purge_reports_files_it_cannot_delete_instead_of_erroring() {
    let vault = vault();
    for name in ["a.md", "b.md", "c.md"] {
        fs::write(vault.path().join(name), name).unwrap();
        delete_note(vault.path(), name).unwrap();
    }

    let trash_dir = vault.path().join(TRASH_DIR);
    chmod(&trash_dir, 0o555);
    let result = trash_purge(vault.path());
    chmod(&trash_dir, 0o755);

    let result = result.expect("删不掉不能返回错误");
    assert_eq!(result.removed, 0);
    assert_eq!(result.failed, 3);
    assert_eq!(trash_entries(&vault).len(), 3, "一篇都没少");
}

/* ───────────────────── 分页 ───────────────────── */

/// 造 `count` 条只有文件、没有清单的条目（时间戳逐条 +1ms）。
fn build_entries(root: &Path, count: usize) -> Vec<String> {
    let trash = root.join(TRASH_DIR);
    fs::create_dir_all(&trash).unwrap();
    let mut names = Vec::new();
    for index in 0..count {
        let name = format!("{}-n{index:03}.md", 1_700_000_000_000i64 + index as i64);
        fs::File::create(trash.join(&name)).unwrap();
        names.push(name);
    }
    names
}

/// 验收 2：97 条、页大小 7 —— 一路用 `next` 取到底，不重不漏。
///
/// # 为什么判据是集合而不是计数
///
/// `seen.len() == 97` 只证明没有重复；要证明没有**漏**，必须与磁盘上的
/// 文件集合比。两者都成立才算「不重不漏」。
#[test]
fn paging_never_repeats_or_drops_an_entry() {
    let vault = vault();
    let names = build_entries(vault.path(), 97);

    let mut seen: Vec<String> = Vec::new();
    let mut cursor = None;
    let mut pages = 0usize;
    loop {
        let page = trash_page(vault.path(), cursor.as_ref(), 7).unwrap();
        assert_eq!(page.total, 97, "total 是总数，不随翻页变化");
        assert!(page.entries.len() <= 7, "一页不许超过 limit");
        seen.extend(page.entries.into_iter().map(|entry| entry.entry));
        pages += 1;
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    assert_eq!(pages, 14, "97 条 / 每页 7 条 = 14 页（最后一页 6 条）");
    assert_eq!(seen.len(), 97, "翻页总数不对：漏了或重了");

    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 97, "翻页返回了重复条目");

    let mut expected = names.clone();
    expected.sort();
    assert_eq!(unique, expected, "翻页拿到的集合必须与磁盘上的文件集合相等");
    assert_eq!(seen.first().unwrap(), names.last().unwrap(), "最新在前");
    assert_eq!(seen.last().unwrap(), names.first().unwrap(), "最旧在最后");
}

/// 验收 3：同一毫秒的两条正好落在页边界上（页大小 1）。
///
/// 没有 tiebreaker 的实现会在第二页把另一条一起排除（`>` 判据）或重复
/// 返回同一条（`>=` 判据）。tiebreaker 必须是 `entry` 而不是展示名：
/// `a/c.md` 与 `b/c.md` 的展示名完全一样。
#[test]
fn entries_deleted_in_the_same_millisecond_cross_a_page_boundary_once() {
    let vault = vault();
    let trash = vault.path().join(TRASH_DIR);
    fs::create_dir_all(&trash).unwrap();
    for name in [
        "1700000000000-a.md",
        "1700000000000-b.md",
        "1700000000001-c.md",
    ] {
        fs::File::create(trash.join(name)).unwrap();
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = trash_page(vault.path(), cursor.as_ref(), 1).unwrap();
        seen.extend(page.entries.into_iter().map(|entry| entry.entry));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    assert_eq!(
        seen,
        vec![
            "1700000000001-c.md",
            "1700000000000-b.md",
            "1700000000000-a.md"
        ],
        "同毫秒的两条各出现一次，顺序由 entry 决定"
    );
}

/// 验收 5：用户手工放进 `trash/` 的无时间戳文件 —— 仍列出、排最后、能恢复。
///
/// 排序键解析失败时按 `0` 处理，不是跳过：那个文件是用户的，他可能正想
/// 把它捞回来。跳过等于让一条真实存在的记录在列表里凭空消失。
#[test]
fn an_entry_without_a_timestamp_prefix_is_listed_last_and_restorable() {
    let vault = vault();
    let trash = vault.path().join(TRASH_DIR);
    fs::create_dir_all(&trash).unwrap();
    fs::write(trash.join("手工放进来的.md"), "手工").unwrap();
    fs::File::create(trash.join("1700000000000-real.md")).unwrap();

    let page = trash_page(vault.path(), None, 10).unwrap();
    assert_eq!(page.total, 2, "没有前缀的条目也要计入 total");
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.entries[0].entry, "1700000000000-real.md");
    assert_eq!(page.entries[1].entry, "手工放进来的.md", "没有前缀的排最后");
    assert_eq!(
        page.entries[1].name, "手工放进来的.md",
        "剥不出前缀就原样返回，不猜"
    );

    let restored = trash_restore(vault.path(), "手工放进来的.md").unwrap();
    assert_eq!(restored.fallback, Some(Fallback::RecordMissing));
    assert_eq!(restored.path, "手工放进来的.md");
    assert_eq!(
        fs::read_to_string(vault.path().join("手工放进来的.md")).unwrap(),
        "手工"
    );
}

/// 清单被手改成别的 `deletedAt`：**排序仍按文件名**，显示值仍取清单。
///
/// # 这条是刻意钉住"不一致"的
///
/// 排序键必须不依赖 IO（否则为了排第一页得读全量清单，分页就不成立），
/// 所以清单里的值改不动顺序。代价是这一条会排在与它显示的时间不一致的
/// 位置上——那是**有意的**，不是 bug。若哪天有人把排序改回"读清单再排"，
/// 这条会红。
#[test]
fn the_order_follows_the_file_name_even_when_the_manifest_says_otherwise() {
    let vault = vault();
    fs::write(vault.path().join("a.md"), "A").unwrap();
    fs::write(vault.path().join("b.md"), "B").unwrap();
    delete_note(vault.path(), "a.md").unwrap();
    delete_note(vault.path(), "b.md").unwrap();

    let names = trash_entries(&vault);
    let older = names
        .iter()
        .find(|name| name.ends_with("-a.md"))
        .expect("回收目录里应当有 a.md")
        .clone();
    fs::write(
        vault
            .path()
            .join(MANIFEST_DIR)
            .join(format!("{older}.json")),
        r#"{"origin":"a.md","deletedAt":9999999999999}"#,
    )
    .unwrap();

    let page = trash_page(vault.path(), None, 10).unwrap();
    assert_eq!(page.entries.len(), 2);
    assert!(
        page.entries[0].entry.ends_with("-b.md"),
        "排序按文件名前缀，b 在后删的那一条上：{:?}",
        page.entries.iter().map(|e| &e.entry).collect::<Vec<_>>()
    );
    assert!(page.entries[1].entry.ends_with("-a.md"));
    assert_eq!(
        page.entries[1].deleted_at, 9_999_999_999_999,
        "显示值仍然取清单（被手改成未来），与排序位置不一致是刻意的"
    );
}

/// 验收 4 的数据层：翻页**期间**恢复一条，后续页不重不漏。
///
/// # 这是 keyset 与 offset 的分水岭
///
/// offset 分页在第 1 页之后删掉其中一条，第二页的 `skip(5)` 会跳过一条
/// （所有行往前挪了一位），用户永远看不到它。keyset 不会：游标指向的是
/// 「上次读到的那个键」，前面少一条与后面无关。
///
/// 这条用例不重新从 `null` 开始翻——那正是前端「恢复后不整页重取」的做法。
#[test]
fn a_restore_between_pages_does_not_shift_the_rest() {
    let vault = vault();
    let names = build_entries(vault.path(), 20);

    let first = trash_page(vault.path(), None, 5).unwrap();
    assert_eq!(first.entries.len(), 5);
    assert!(first.next.is_some());

    // 模拟"用户在已加载的列表里恢复了第一条"：文件离开回收目录。
    let restored = first.entries[0].entry.clone();
    fs::remove_file(vault.path().join(TRASH_DIR).join(&restored)).unwrap();

    // 用第一页的 `next` 继续翻，**不**从 null 重来。已加载的那 5 条里
    // 去掉刚恢复的一条，剩下的 4 条本来就还在列表里——前端也是这样算的。
    let mut seen: Vec<String> = first.entries[1..]
        .iter()
        .map(|entry| entry.entry.clone())
        .collect();
    let mut cursor = first.next;
    while let Some(position) = cursor {
        let page = trash_page(vault.path(), Some(&position), 5).unwrap();
        seen.extend(page.entries.into_iter().map(|entry| entry.entry));
        cursor = page.next;
    }

    let mut expected = names;
    expected.sort();
    expected.retain(|name| name != &restored);
    let mut got = seen.clone();
    got.sort();

    assert_eq!(seen.len(), 19, "20 条里恢复掉 1 条，剩下的必须都翻到");
    assert_eq!(got, expected, "恢复一条之后翻页必须不重不漏");
}

/* ───────────────────── 崩溃（kill -9） ───────────────────── */

const CHILD_VAULT: &str = "KI_TRASH_CHILD_VAULT";

/// 子进程主体：不停"写一篇 → 删一篇"，每删完一条报一声。
///
/// 不带 `KI_TRASH_CHILD_VAULT` 时立刻返回——它在普通 `cargo test` 里
/// 只是一条无副作用的空用例（与 `tests/crash.rs` 同一手法）。
#[test]
fn child_deleter() {
    let Ok(vault) = std::env::var(CHILD_VAULT) else {
        return;
    };
    let root = PathBuf::from(vault);
    for index in 0..400 {
        let rel = format!("n{index:03}.md");
        fs::write(root.join(&rel), "x").expect("子进程写文件失败");
        let trashed = delete_note(&root, &rel).expect("子进程删除失败");
        println!("DELETED {rel} {trashed}");
    }
}

/// 极简伪随机：只要够"落在删除流程的不同阶段"，不需要统计学质量。
fn pseudo_random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

/// 验收 4：删完立刻硬杀 → 重启后列表仍能看到那一条，且原路径还在。
///
/// # 这条验的是什么
///
/// 不是原子性（见 `manifest_records_are_never_observed_half_written`），
/// 而是**落盘时机**：`delete_note` 返回之前，记录必须已经写完。
/// 若哪天有人把记录改成"攒一批再写"或"退出时统一写"，这条会红。
///
/// # 怎么跑
///
/// 默认被 `#[ignore]` 跳过（它是时序相关的外部进程测试）：
///
/// ```sh
/// KI_CRASH_ROUNDS=20 cargo test -p ki-vault --test trash -- --ignored --nocapture
/// ```
#[test]
#[ignore = "时序相关的外部进程测试，用 KI_CRASH_ROUNDS=20 cargo test -p ki-vault --test trash -- --ignored 显式运行"]
fn a_hard_kill_right_after_a_delete_still_lists_the_entry() {
    let rounds: u32 = std::env::var("KI_CRASH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(20);

    let exe = std::env::current_exe().expect("拿不到测试二进制路径");
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;

    let mut killed = 0u32;

    for round in 1..=rounds {
        let vault = TempDir::new().expect("建临时 Vault 失败");

        let mut child = Command::new(&exe)
            .args(["--exact", "child_deleter", "--nocapture"])
            .env(CHILD_VAULT, vault.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动子进程失败");

        let stdout = child.stdout.take().expect("子进程没有 stdout");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("DELETED ") {
                    // `DELETED <rel> <trash 相对路径>`：只要 rel，它就是要断言的原路径。
                    if let Some(rel) = rest.split_whitespace().next() {
                        if tx.send(rel.to_owned()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let first = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("子进程 30 秒内一次都没删成，无法判定");

        // 随机再等 0–60ms：一次删除（rename + 写清单 + fsync）在本机约 0.5–3ms，
        // 这个区间能让硬杀落在删除流程的不同阶段，而不是每次都落在第一轮之后。
        let extra = pseudo_random(&mut seed) % 60;
        std::thread::sleep(Duration::from_millis(extra));

        // 子进程可能已经自己跑完（400 条很快）。杀一个已退出的进程不算错，
        // 这一轮照样有效：它已经证明"至少一条删完了"。
        let _ = child.kill();
        let _ = child.wait();

        // 把管道读干净：杀掉之后 stdout 里还可能有已写完的行。
        let mut reported: Vec<String> = vec![first];
        reported.extend(rx.try_iter());

        // ── 重启后校验（新进程、新读取，只有磁盘上的东西可见） ──
        let listed = trash_list(vault.path()).unwrap();
        for rel in &reported {
            let entry = listed
                .iter()
                .find(|entry| entry.origin.as_deref() == Some(rel.as_str()))
                .unwrap_or_else(|| {
                    panic!(
                        "第 {round} 轮：`{rel}` 的删除已经返回，重启后列表里却没有它。\
                         当前列表：{:?}",
                        listed.iter().map(|e| &e.entry).collect::<Vec<_>>()
                    )
                });
            assert!(entry.size > 0, "第 {round} 轮：`{rel}` 的大小不对");
        }

        // 清单里不许出现"半截 JSON"：那是 `fs::write` 被硬杀之后的签名。
        for name in manifest_entries(vault.path()) {
            let bytes = fs::read(vault.path().join(MANIFEST_DIR).join(&name)).unwrap();
            assert!(
                !bytes.is_empty() && bytes.first() == Some(&b'{') && bytes.last() == Some(&b'}'),
                "第 {round} 轮：清单记录 {name} 是半截的（{} 字节）",
                bytes.len()
            );
        }

        killed += 1;
        println!(
            "round {round:>2}: reported={:<3} listed={:<3} delay={extra:>2}ms",
            reported.len(),
            listed.len()
        );
    }

    println!("=== 硬杀：{killed}/{rounds} 轮，0 轮丢失已完成的删除 ===");
}
