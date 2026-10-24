//! 自动保存的行为测试。
//!
//! 与 `tests/commands.rs` 的分工：那边验「接线对不对」，这边验**保存策略**——
//! 什么时候落盘、落了几次、冲突了怎么办。
//!
//! # 为什么这些用例必须串行
//!
//! 自动保存的槽位是进程级单例（应用里本来就只有一个编辑器）。
//! `cargo test` 默认多线程跑，两个用例同时改同一个槽位会互相污染。
//! 所以每个用例开头都拿一把全局锁，并重置槽位。
//!
//! # 为什么允许真的等 1.5 秒
//!
//! 只有一条用例需要等：验证「不调 flush 也会自动落盘」。用真实间隔而不是
//! 缩短的间隔，是因为**被测的就是这个数字**——把间隔改成 10ms 再断言 10ms，
//! 等于什么都没验。

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ki_core::KiError;
use ki_ipc::{autosave, note_edit, note_flush, note_save};
use tempfile::TempDir;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tauri::async_runtime::block_on(future)
}

static SERIAL: Mutex<()> = Mutex::new(());

/// 拿全局锁并重置槽位。返回的 guard 活到用例结束。
fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    autosave::reset_for_test();
    guard
}

fn root_of(vault: &TempDir) -> String {
    vault.path().to_string_lossy().into_owned()
}

fn disk(vault: &TempDir, rel: &str) -> String {
    std::fs::read_to_string(vault.path().join(rel)).unwrap_or_default()
}

/// 同步写一篇笔记，作为测试的起点（绕过自动保存）。
fn seed(root: &str, rel: &str, content: &str) -> ki_core::SaveResult {
    block_on(note_save(
        root.to_owned(),
        rel.to_owned(),
        content.to_owned(),
    ))
    .expect("播种写入失败")
}

/// 提交一次编辑，返回状态。
fn edit(root: &str, rel: &str, content: &str, base_mtime: Option<i64>) -> ki_core::SaveState {
    block_on(note_edit(
        root.to_owned(),
        rel.to_owned(),
        content.to_owned(),
        base_mtime,
    ))
    .expect("note_edit 不该失败")
}

/* ───────────────────── 基本路径 ───────────────────── */

/// `note_edit` 之后不显式 flush，内容也必须真的到磁盘上。
///
/// 这条是整个自动保存功能的存在理由：用户从不按保存键。
#[test]
fn autosave_lands_without_an_explicit_flush() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "OLD");

    edit(&root, "a.md", "NEW", None);

    // 间隔是 1.5 秒；给到 3 秒是为了容忍 CI 上的调度抖动。
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if disk(&vault, "a.md") == "NEW" {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("3 秒内没有自动落盘，节流器或定时线程没工作");
}

/// `note_flush` 立刻落盘，不等节流周期。
#[test]
fn flush_writes_immediately() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "OLD");

    edit(&root, "a.md", "FLUSHED", None);
    let saved = block_on(note_flush(root, "a.md".to_owned(), false))
        .expect("flush 应当成功")
        .expect("有未落盘内容，应当返回 Some");

    assert_eq!(saved.size, 7);
    assert_eq!(disk(&vault, "a.md"), "FLUSHED");
}

/// 没有待落盘内容时，flush 返回 `None` 而不是伪造一个结果。
#[test]
fn flush_without_pending_content_returns_none() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "OLD");

    // 换文档会先同步落盘旧内容，所以这里 flush 时已经没有 pending 了。
    edit(&root, "a.md", "SYNCED", None);
    block_on(note_flush(root.clone(), "a.md".to_owned(), false)).unwrap();

    let again = block_on(note_flush(root, "a.md".to_owned(), false)).unwrap();
    assert!(again.is_none(), "没有未落盘内容时应当返回 None");
}

/// 连续提交时，磁盘上最终必须是**最后一次**的内容，不是中间某一版。
#[test]
fn continuous_edits_end_up_with_the_latest_content() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "");

    for i in 1..=20 {
        edit(&root, "a.md", &"x".repeat(i), None);
        std::thread::sleep(Duration::from_millis(10));
    }
    block_on(note_flush(root, "a.md".to_owned(), false)).unwrap();

    assert_eq!(
        disk(&vault, "a.md").len(),
        20,
        "磁盘上应当是最新那一次的内容"
    );
}

/* ───────────────────── 冲突 ───────────────────── */

/// 外部程序改过文件后，自动保存必须**报冲突且不覆盖**。
///
/// 这是自动保存最危险的一类事故：没有乐观锁的话，用户在 Obsidian 里
/// 刚保存的内容会被知屿 1.5 秒后静默抹掉。
#[test]
fn autosave_refuses_to_overwrite_an_externally_modified_note() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);

    let opened = block_on(note_save(
        root.clone(),
        "a.md".to_owned(),
        "MINE".to_owned(),
    ))
    .unwrap();

    // 模拟外部编辑器：mtime 必须变，否则 1 秒精度的文件系统上看不出差别。
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();

    // 前端打开时读到的是 opened.mtime，编辑后提交。
    let state = edit(&root, "a.md", "MINE-EDITED", Some(opened.mtime));
    assert!(state.dirty, "冲突时内容仍未落盘，dirty 必须为 true");

    let err = block_on(note_flush(root.clone(), "a.md".to_owned(), false))
        .expect_err("基线过期必须报冲突");
    assert!(
        matches!(err, KiError::Conflict { .. }),
        "应为 Conflict，实际 {err:?}"
    );

    assert_eq!(disk(&vault, "a.md"), "THEIRS", "冲突时绝不能覆盖外部内容");
}

/// 冲突之后用户选择「用我的版本覆盖」：`force` 跳过乐观锁。
#[test]
fn force_flush_overwrites_after_a_conflict() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);

    let opened = block_on(note_save(
        root.clone(),
        "a.md".to_owned(),
        "MINE".to_owned(),
    ))
    .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();

    edit(&root, "a.md", "MINE-WINS", Some(opened.mtime));
    assert!(block_on(note_flush(root.clone(), "a.md".to_owned(), false)).is_err());

    // 冲突后内容仍留在槽位里，所以 force 能把它写出去。
    let saved = block_on(note_flush(root, "a.md".to_owned(), true))
        .expect("force 应当成功")
        .expect("内容还在槽位里");
    assert_eq!(saved.size, 9);
    assert_eq!(disk(&vault, "a.md"), "MINE-WINS");
}

/// 冲突解决（force）之后，后续的自动保存不该再报冲突。
///
/// 锁的是「基线会随成功落盘向前推进」这条不变量：如果 force 写成功后
/// 基线还停在旧值，用户接下来每打一个字都会被报一次冲突。
#[test]
fn baseline_advances_after_a_forced_save() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);

    let opened = block_on(note_save(
        root.clone(),
        "a.md".to_owned(),
        "MINE".to_owned(),
    ))
    .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();

    edit(&root, "a.md", "MINE-WINS", Some(opened.mtime));
    block_on(note_flush(root.clone(), "a.md".to_owned(), true)).unwrap();

    // 继续编辑：不该再冲突。
    edit(&root, "a.md", "MINE-WINS-2", None);
    block_on(note_flush(root.clone(), "a.md".to_owned(), false))
        .expect("force 之后基线已推进，不该再冲突");
    assert_eq!(disk(&vault, "a.md"), "MINE-WINS-2");
}

/* ───────────────────── 切换文档 ───────────────────── */

/// 切到另一篇笔记时，上一篇必须被同步落盘。
///
/// 否则「切走」就是一次静默的数据丢失，而且用户完全看不出发生了什么。
#[test]
fn switching_documents_flushes_the_previous_one() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "A0");
    seed(&root, "b.md", "B0");

    edit(&root, "a.md", "A-EDITED", None);
    // 不 flush，直接切到 b。
    edit(&root, "b.md", "B-EDITED", None);

    assert_eq!(
        disk(&vault, "a.md"),
        "A-EDITED",
        "切换文档时上一篇必须已经落盘"
    );
}

/// 切换文档会重置状态：`last_saved_mtime` 不该把上一篇的时间戳带到新文档上。
#[test]
fn switching_documents_resets_the_reported_state() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "A");
    seed(&root, "b.md", "B");

    edit(&root, "a.md", "A1", None);
    block_on(note_flush(root.clone(), "a.md".to_owned(), false)).unwrap();

    let state = edit(&root, "b.md", "B1", None);
    assert_eq!(
        state.last_saved_mtime, None,
        "新文档还没落过盘，不该继承上一篇的 mtime"
    );
}

/* ───────────────────── 路径边界 ───────────────────── */

/// 自动保存不能成为绕过路径校验的后门。
#[test]
fn autosave_still_enforces_the_vault_boundary() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);

    edit(&root, "../escape.md", "x", None);
    let err = block_on(note_flush(root, "../escape.md".to_owned(), false))
        .expect_err("路径逃逸必须被拒绝");
    assert!(format!("{err:?}").contains("PathEscape"), "实际 {err:?}");
    assert!(!Path::new(vault.path().parent().unwrap())
        .join("escape.md")
        .exists());
}
