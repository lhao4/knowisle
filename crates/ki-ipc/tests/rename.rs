//! 改名与自动保存槽位的交互。
//!
//! 这一组用例只回答一个问题：**改名之后，槽位里那份还没落盘的内容会不会
//! 写回已经不存在的旧路径**。这是本轮唯一"数据会分裂成两份"的风险点：
//! 新文件停在旧内容、旧文件复活，而用户以为自己在编辑改名后的那一篇。
//!
//! # 为什么必须串行
//!
//! 槽位是进程级单例（理由见 `tests/autosave.rs` 的文件头）。

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ki_core::KiError;
use ki_ipc::{autosave, note_edit, note_flush, note_rename, note_save, note_status};
use tempfile::TempDir;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tauri::async_runtime::block_on(future)
}

static SERIAL: Mutex<()> = Mutex::new(());

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

fn seed(root: &str, rel: &str, content: &str) -> ki_core::SaveResult {
    block_on(note_save(
        root.to_owned(),
        rel.to_owned(),
        content.to_owned(),
    ))
    .expect("播种写入失败")
}

fn edit(root: &str, rel: &str, content: &str, base_mtime: Option<i64>) -> ki_core::SaveState {
    block_on(note_edit(
        root.to_owned(),
        rel.to_owned(),
        content.to_owned(),
        base_mtime,
    ))
    .expect("note_edit 不该失败")
}

/* ───────────────────── 坑 1：槽位会让旧文件复活 ───────────────────── */

/// 槽位里有 pending 时改名：**先落盘，再改名**，旧路径不许复活。
///
/// 顺序反了的话，worker 会把"改名前的正文"写到已经不存在（被 rename 掉）的
/// 旧路径上——`fs::rename` 之后旧路径本来是不存在的，一次 write 就把它变回来。
#[test]
fn rename_flushes_pending_content_before_touching_the_directory_entry() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "旧内容");

    edit(&root, "a.md", "改名前的正文", None);

    let next = block_on(note_rename(root.clone(), "a.md".to_owned(), "b".to_owned()))
        .expect("改名应当成功");
    assert_eq!(next, "b.md");

    assert_eq!(disk(&vault, "b.md"), "改名前的正文", "内容必须跟着文件走");
    assert!(
        !vault.path().join("a.md").exists(),
        "旧路径必须不存在——它一旦存在，就是 worker 复活出来的"
    );
}

/// 改名之后继续编辑：内容必须落到**新路径**，旧路径仍然不许出现。
///
/// 这条锁的是 `rename_slot`：只 flush 不搬槽位的话，下一次 `note_edit` 会把
/// 键切回旧路径（"换了一篇笔记"），worker 于是把新内容写进旧文件。
#[test]
fn editing_after_a_rename_writes_to_the_new_path_only() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "v1");

    block_on(note_rename(root.clone(), "a.md".to_owned(), "b".to_owned())).unwrap();

    edit(&root, "b.md", "v2", None);
    block_on(note_flush(root.clone(), "b.md".to_owned(), false)).expect("flush 应当成功");

    assert_eq!(disk(&vault, "b.md"), "v2");
    assert!(!vault.path().join("a.md").exists(), "旧路径被复活了");
}

/// 改名之后 `note_status` 必须认新路径。
///
/// 槽位键没搬的话，状态查询会返回"这篇没有未落盘内容"——前端于是把
/// 「保存中…」变成空白，而内容其实还压在槽位里。
#[test]
fn status_follows_the_note_to_its_new_path() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "v1");

    block_on(note_rename(root.clone(), "a.md".to_owned(), "b".to_owned())).unwrap();

    // 改完名立刻编辑：此刻槽位键必须是 b.md，否则这次编辑会被判成"换文档"。
    let state = edit(&root, "b.md", "v2", None);
    assert!(state.dirty, "刚提交的内容应当处于未落盘状态");
    assert!(
        block_on(note_status(root.clone(), "b.md".to_owned()))
            .expect("status 不该失败")
            .dirty
    );
    block_on(note_flush(root, "b.md".to_owned(), false)).unwrap();
    assert_eq!(disk(&vault, "b.md"), "v2");
}

/// worker 已经取走内容、正在写盘时改名：也要等它写完。
///
/// `drain_for_rename` 里那句 `flush` 的等待分支就是为这条存在的。少了它，
/// 改名会与写盘并发：写盘用的是旧路径的绝对路径（job 里存的是相对路径 +
/// root，解析发生在 write 那一刻），于是旧文件被重新创建。
#[test]
fn rename_waits_for_a_worker_that_already_took_the_job() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "");

    let big = "x".repeat(1_000_000);
    edit(&root, "a.md", &big, None);

    // 等 worker 把 pending 取走（此后它要么正在写，要么刚写完）。
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if !autosave::debug_state_for_test().0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !autosave::debug_state_for_test().0,
        "5 秒内 worker 都没把内容取走"
    );

    block_on(note_rename(root, "a.md".to_owned(), "b".to_owned())).expect("改名应当成功");

    assert_eq!(disk(&vault, "b.md").len(), 1_000_000, "内容必须跟着走");
    assert!(
        !vault.path().join("a.md").exists(),
        "旧路径被 worker 复活了"
    );
}

/* ───────────────────── 失败路径 ───────────────────── */

/// 冲突时**不许改名**。
///
/// 改名会把"写不出去的内容"搬到新路径上：用户看到的是"改名之后内容回退了"，
/// 而冲突横幅还指着旧路径。让改名失败是唯一不会骗人的选择。
#[test]
fn rename_refuses_to_proceed_when_the_note_cannot_be_flushed() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let opened = seed(&root, "a.md", "MINE");

    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();
    edit(&root, "a.md", "MINE-EDITED", Some(opened.mtime));

    let err = block_on(note_rename(root, "a.md".to_owned(), "b".to_owned()))
        .expect_err("落盘失败时必须拒绝改名");

    assert!(matches!(err, KiError::Conflict { .. }), "实际 {err:?}");
    assert_eq!(disk(&vault, "a.md"), "THEIRS", "磁盘上一个字节都不该动");
    assert!(!vault.path().join("b.md").exists());
}

/// 改的不是当前文档：槽位里那一篇的内容不许被顺手写一遍。
///
/// 用户完全可能在树里右键改一篇没打开的笔记的名字。此时若去 drain，
/// 会把**另一篇**的 pending 落盘——内容没错，但时机错了：它可能正在被编辑。
#[test]
fn renaming_an_unopened_note_does_not_touch_the_slot() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "active.md", "A0");
    seed(&root, "other.md", "O0");

    edit(&root, "active.md", "A-EDIT", None);

    block_on(note_rename(
        root.clone(),
        "other.md".to_owned(),
        "renamed".to_owned(),
    ))
    .expect("改名应当成功");
    assert_eq!(disk(&vault, "renamed.md"), "O0");

    // 槽位仍然属于 active.md：flush 必须能把它的内容写下去。
    block_on(note_flush(root, "active.md".to_owned(), false)).expect("flush 应当成功");
    assert_eq!(disk(&vault, "active.md"), "A-EDIT");
}
