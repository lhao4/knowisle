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
//! # 原子性怎么验
//!
//! `kill -9` **证明不了**原子写：进程死了，页缓存还在，`fs::write` 写下去
//! 的字节一样会在重启后读到。真正的判据是「读者永远看不到半个文件」——
//! 见 [`manifest_records_are_never_observed_half_written`]，那是把
//! `atomic_replace` 换成 `fs::write` 之后唯一会变红的用例。

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ki_core::KiError;
use ki_vault::{
    delete_note, read_note, trash_list, trash_purge, trash_restore, MANIFEST_DIR, TRASH_DIR,
};
use tempfile::TempDir;

fn vault() -> TempDir {
    TempDir::new().unwrap()
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
    assert_eq!(trash_purge(vault.path()).unwrap(), 0);
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
    assert!(!result.degraded);
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

/// 验收 5：清单写坏 → 列表只显示文件名，恢复落到库根目录并标记降级。
///
/// "写坏"用的是最真实的一种：文件在，但内容不是合法 JSON（同步工具截断、
/// 磁盘坏块、用户手改）。整条记录作废，但**文件本身仍然可用**。
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
    assert!(result.degraded);
    assert_eq!(result.path, "c.md", "降级后落在库根目录");
    assert_eq!(
        fs::read_to_string(vault.path().join("c.md")).unwrap(),
        "内容"
    );
}

/// 清单里的原路径被改成越界路径：**不显示、不执行**，按降级处理。
///
/// 清单是磁盘上的普通文件，任何人都能改它。`../../.ssh/id_rsa` 若被当成
/// 原路径显示出来，用户会以为自己的笔记原本在那儿；若被拿去恢复，
/// 就是一次跨库搬运。
#[test]
fn a_tampered_origin_is_treated_as_missing() {
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
    assert!(result.degraded);
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
    assert_eq!(
        trash_restore(vault.path(), &listed[0].entry).unwrap().path,
        "a.md"
    );
}

/// 崩溃在"文件已搬走、清单还没写"之间：条目仍在，恢复落到根目录。
///
/// 这正是删除的两个步骤之间那个窗口。造法就是删掉记录文件本身——
/// 与"崩溃时记录还没落盘"在磁盘上是同一个状态。
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
    assert!(result.degraded);
    assert_eq!(result.path, "c.md", "从文件名里剥出原名，落在库根");
    assert!(vault.path().join("c.md").is_file());
}

/// 原目录在外部被删掉了 → 不重建目录，落到库根并标记降级。
///
/// "悄悄重建一串目录"比"落到根目录并明确告知"更容易让用户困惑：
/// 他会以为那个目录一直都在。
#[test]
fn restore_falls_back_to_the_root_when_the_original_directory_is_gone() {
    let vault = vault();
    fs::create_dir_all(vault.path().join("gone")).unwrap();
    fs::write(vault.path().join("gone/c.md"), "内容").unwrap();
    delete_note(vault.path(), "gone/c.md").unwrap();
    fs::remove_dir_all(vault.path().join("gone")).unwrap();

    let result = trash_restore(vault.path(), &trash_entries(&vault)[0]).unwrap();

    assert!(result.degraded);
    assert_eq!(result.path, "c.md");
    assert!(
        !vault.path().join("gone").exists(),
        "不代建原目录：那是外部删除，不是我们的决定"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("c.md")).unwrap(),
        "内容"
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
#[test]
fn restore_refuses_entry_names_that_escape_the_trash_directory() {
    let vault = vault();
    fs::create_dir_all(vault.path().join(TRASH_DIR)).unwrap();
    fs::write(vault.path().join(TRASH_DIR).join("x.md"), "x").unwrap();

    for bad in ["../x.md", "..", ".", "a/b.md", "a\\b.md", "/x.md"] {
        let err = trash_restore(vault.path(), bad).expect_err(bad);
        assert!(
            matches!(err, KiError::PathEscape { .. }),
            "{bad} 应当是 PathEscape，实际 {err:?}"
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

    let removed = trash_purge(vault.path()).unwrap();
    assert_eq!(removed, 1, "只数删掉的文件，不数清单目录");
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

    let removed = trash_purge(vault.path()).unwrap();

    assert_eq!(removed, 3);
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
