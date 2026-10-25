//! 删除与自动保存槽位的交互。
//!
//! 这一组用例只回答一个问题：**删除之后，槽位里那份还没落盘的内容会不会
//! 把文件写回来**。这是本轮唯一"删了等于没删"的风险点：文件进了 trash，
//! 一个迟到的 flush 又在原路径上造出一个新文件——用户看到的是"删除没生效"，
//! 而真正发生的是"删除生效了，然后被撤销了"。
//!
//! # 为什么必须串行
//!
//! 槽位是进程级单例（理由见 `tests/autosave.rs` 的文件头）。

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ki_core::KiError;
use ki_ipc::{
    autosave, note_create, note_delete, note_edit, note_flush, note_read, note_save, note_status,
};
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

fn delete(root: &str, rel: &str) -> Result<String, KiError> {
    block_on(note_delete(root.to_owned(), rel.to_owned()))
}

/* ───────────────────── 坑 1：删掉的必须是"最新版本" ───────────────────── */

/// 槽位里有 pending 时删除：**先落盘，再搬目录项**。
///
/// 顺序反了的话，trash 里那份是"上一次落盘的旧内容"——用户最后 1.5 秒打的字
/// 既不在原路径（文件没了），也不在 trash（那是旧版）。这是删除路径上
/// 唯一会**静默**丢数据的分支。
#[test]
fn delete_flushes_pending_content_into_trash_before_moving_it() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "旧内容");

    edit(&root, "a.md", "旧内容 还没落盘的标记", None);

    let trashed = delete(&root, "a.md").expect("删除应当成功");

    assert!(
        disk(&vault, &trashed).contains("还没落盘的标记"),
        "trash 里必须是最后一次编辑的内容，实际：{}",
        disk(&vault, &trashed)
    );
    assert!(!vault.path().join("a.md").exists(), "原路径必须不存在");
}

/// 冲突时**不许删除**。
///
/// 此刻用户手里还有「覆盖 / 放弃」两个选择；删掉就只剩一个，而剩下的那个
/// 还不是他选的。磁盘上的对方版本一个字节都不该动。
#[test]
fn delete_refuses_to_proceed_when_the_note_cannot_be_flushed() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let opened = seed(&root, "a.md", "MINE");

    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();
    edit(&root, "a.md", "MINE-EDITED", Some(opened.mtime));

    let err = delete(&root, "a.md").expect_err("落盘失败时必须拒绝删除");

    assert!(matches!(err, KiError::Conflict { .. }), "实际 {err:?}");
    assert_eq!(disk(&vault, "a.md"), "THEIRS", "磁盘上一个字节都不该动");
    assert!(
        !vault.path().join(".knowisle/trash").exists(),
        "拒绝删除时不该建出回收目录"
    );
}

/// worker 已经取走内容、正在写盘时删除：也要等它写完。
///
/// `drain_for_delete` 里那句 `flush` 的等待分支就是为这条存在的。少了它，
/// 删除会与写盘并发：写盘用的是旧路径，于是 trash 里少几个字节、或者
/// 原路径上多出一个只有尾巴的残文件。
#[test]
fn delete_waits_for_a_worker_that_already_took_the_job() {
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

    let trashed = delete(&root, "a.md").expect("删除应当成功");

    assert_eq!(
        disk(&vault, &trashed).len(),
        1_000_000,
        "trash 里必须是完整的 1MB，不是写了一半的残文件"
    );
    assert!(
        !vault.path().join("a.md").exists(),
        "原路径被 worker 复活了"
    );
}

/* ───────────────────── 坑 2：迟到的 flush 不许复活文件 ───────────────────── */

/// 删除之后任何迟到的 flush 都是无操作。
///
/// 前端 `discardPending` 只挡得住 300ms 合并窗口里的那一次 `noteEdit`；
/// 挡不住已经在飞的 `noteFlush`。`clear_for_delete` 把 `key` 置 None 之后，
/// `flush` 的第一道判断就直接返回——这条是最后一道防线。
#[test]
fn a_late_flush_after_delete_does_not_resurrect_the_file() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "内容");
    edit(&root, "a.md", "内容 v2", None);

    delete(&root, "a.md").expect("删除应当成功");

    // 模拟"删除之前就出发、删除之后才到达"的那次冲刷。
    let late = block_on(note_flush(root.clone(), "a.md".to_owned(), false));
    assert!(
        matches!(late, Ok(None)),
        "迟到的 flush 必须是空操作，实际 {late:?}"
    );
    assert!(!vault.path().join("a.md").exists(), "文件被复活了");

    // 状态查询同样不该抛，也不该说"还有内容没落盘"。
    let state = block_on(note_status(root, "a.md".to_owned())).expect("status 不该失败");
    assert!(!state.dirty, "槽位已清空，不该还有 dirty");
}

/* ───────────────────── 坑 3：槽位清空，不是搬键 ───────────────────── */

/// 验收 11：删 `a.md` → 新建 `a.md` → 打开 → 打字 → flush，**第一次保存不许冲突**。
///
/// # 这条为什么能抓住"忘记清槽位"
///
/// 槽位记着 `(root, path)` 与乐观锁基线。若删除后只清 `pending`、留下 `key`，
/// 那么重新打开同一路径时 `submit` 会认为"还是同一篇文档"，于是**沿用旧基线**
/// ——而那个 mtime 属于已经被搬走的那个文件。第一次写盘必然 `CONFLICT`，
/// 用户看到的是"我刚新建的笔记，一打字就冲突"。
#[test]
fn deleting_and_recreating_the_same_path_saves_without_a_conflict() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "第一版");
    edit(&root, "a.md", "第一版 未落盘", None);
    delete(&root, "a.md").expect("删除应当成功");

    // 新建 → 打开（拿 mtime）→ 打字 → flush，与前端 `openNote` 的顺序一致。
    block_on(note_create(root.clone(), String::new(), "a".to_owned())).expect("新建应当成功");
    let opened = block_on(note_read(root.clone(), "a.md".to_owned())).expect("读取应当成功");
    edit(&root, "a.md", "第二版", Some(opened.mtime));

    let flushed = block_on(note_flush(root, "a.md".to_owned(), false));
    assert!(flushed.is_ok(), "第一次保存不许冲突，实际 {flushed:?}");
    assert_eq!(disk(&vault, "a.md"), "第二版");
}

/// 删的不是当前文档：槽位里那一篇的内容不许被顺手写一遍，也不许被清掉。
#[test]
fn deleting_an_unopened_note_does_not_touch_the_slot() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "active.md", "A0");
    seed(&root, "other.md", "O0");

    edit(&root, "active.md", "A-EDIT", None);

    delete(&root, "other.md").expect("删除应当成功");

    // 槽位仍然属于 active.md：flush 必须能把它的内容写下去。
    block_on(note_flush(root, "active.md".to_owned(), false)).expect("flush 应当成功");
    assert_eq!(disk(&vault, "active.md"), "A-EDIT");
}
