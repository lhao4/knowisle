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
use ki_ipc::{autosave, note_edit, note_flush, note_read, note_save};
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

/// 没有待落盘内容时，flush 报告**磁盘现状**，而不是一句"没东西可写"。
///
/// 这条契约的用处：调用方（切换笔记、关窗前）只想知道"我的内容在不在磁盘上"。
/// 返回 `None` 会让它以为失败了，而实际上内容早就被自动保存写完了——
/// worker 抢先落盘是常态，不是异常。
#[test]
fn flush_reports_the_on_disk_state_when_nothing_is_pending() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "OLD");

    edit(&root, "a.md", "SYNCED", None);
    let first = block_on(note_flush(root.clone(), "a.md".to_owned(), false))
        .expect("flush 应当成功")
        .expect("提交过内容，应当有落盘结果");

    // 等自动保存自己写一遍，确保这次 flush 面对的是"真的没有 pending"。
    // 不加这个等待，用例是否覆盖到目标分支取决于线程调度——那就成了运气测试。
    std::thread::sleep(Duration::from_millis(200));

    // 第二次 flush：内容已经在磁盘上，返回同一份 stat。
    let again = block_on(note_flush(root, "a.md".to_owned(), false))
        .expect("flush 应当成功")
        .expect("磁盘上有内容，应当返回它的 stat");
    assert_eq!(again.mtime, first.mtime, "没有新写入时 mtime 不该变");
    assert_eq!(disk(&vault, "a.md"), "SYNCED");
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

/// **切换时 worker 正在写旧文档**，旧内容也必须落盘。
///
/// 这条锁的是 `drain_previous` 的存在理由：worker 已经 `take` 走了 pending
/// 正在写盘，此时若直接切换，槽位就指向了新文档，而旧文档写没写完全没人管。
/// 更糟的是 worker 写完后会把"成功"记到新文档头上。
///
/// 用 1MB 正文把写盘时间拉长，再用 `debug_state_for_test` 等到 worker 把内容
/// 取走之后才切换——`edit` 一返回就切换的话，worker 往往还没被调度起来，
/// 目标分支根本走不到（变异验证时确认过这一点）。
#[test]
fn switching_while_the_worker_is_mid_write_still_flushes_the_previous_note() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "");
    seed(&root, "b.md", "");

    let big = "x".repeat(1_000_000);
    edit(&root, "a.md", &big, None);

    // 等 worker 把内容取走：此后它要么正在写，要么刚写完。
    // 不再要求"恰好抓到正在写"那一瞬——写盘只要几毫秒，抓那一瞬会变成 flaky，
    // 而"取走之后"这个状态已经足够让切换与落盘重叠。
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if !autosave::debug_state_for_test().0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        !autosave::debug_state_for_test().0,
        "5 秒内 worker 都没把内容取走，自动保存的定时线程没工作"
    );

    // 此刻切换：pending 已空，但 worker 手里可能还有活。
    edit(&root, "b.md", "B", None);

    assert_eq!(
        disk(&vault, "a.md").len(),
        1_000_000,
        "切换文档时必须等旧文档写完，否则 worker 的成果会记到新文档头上"
    );
}

/// 切换文档时上一篇写盘失败，**不能变成死循环**。
///
/// `drain_previous` 失败分支的回归用例。`write_job` 出错时会把 job 放回槽位，
/// 若 drain 用 `continue` 重试，下一轮就会把它原样取出来再写一次，
/// 同样的内容配同样的基线必然得到同样的错误——永远出不来。
///
/// 这条在 Windows CI 上真实发生过：`Test (ki-ipc)` 挂满 13 分钟没动。
/// 触发条件是「上一个用例的 worker 在槽位被重置后才写完」，它把旧文档的
/// mtime 记进新槽位，于是切换时的写盘必然冲突。
#[test]
fn switching_away_from_a_failing_note_does_not_spin() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let opened = seed(&root, "a.md", "MINE");
    seed(&root, "b.md", "B0");

    // 外部改过 → 前端手里的基线过期 → 写盘必然冲突。
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();

    edit(&root, "a.md", "MINE-EDITED", Some(opened.mtime));
    assert!(
        block_on(note_flush(root.clone(), "a.md".to_owned(), false)).is_err(),
        "基线过期必须报冲突"
    );

    // 此刻 pending 里躺着一份写不出去的内容。切换必须立刻返回。
    // 放到另一个线程里做，这样死循环表现为「超时失败」而不是「测试挂住」——
    // 挂住的用例在 CI 上只会撞 job 超时，看不出是哪一条。
    let (tx, rx) = std::sync::mpsc::channel();
    let switched_root = root.clone();
    std::thread::spawn(move || {
        let _ = tx.send(edit(&switched_root, "b.md", "B-EDITED", None));
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("切换文档时上一篇写盘失败，drain_previous 陷入死循环");
}

/* ───────────────────── 关窗落盘 ───────────────────── */

/// 关窗路径用的 `flush_slot()` 必须把槽位里的内容写下去。
///
/// 它没有参数：`(root, path)` 从槽位自己取。这条用例锁的就是「自己取」——
/// 一旦有人给它加参数并要求调用方传入，关窗回调就得凭空知道当前是哪一篇，
/// 而那是不可能的（窗口事件里没有文档信息）。
#[test]
fn flush_slot_writes_whatever_the_slot_holds() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "OLD");

    edit(&root, "a.md", "CLOSED", None);
    // 不等 1.5 秒：关窗就发生在节流窗口之内，这正是要覆盖的场景。
    autosave::flush_slot();

    assert_eq!(disk(&vault, "a.md"), "CLOSED");
}

/// 槽位为空（从没编辑过）时，`flush_slot()` 必须立刻返回且不碰磁盘。
///
/// 关窗是用户已经不耐烦才按的按钮，这里多一次 `stat` 都是可见的卡顿。
/// 判据用耗时而不是「有没有调用 fs」：后者要打桩，而这条路径的价值就在于
/// **什么都不做**，能观测到的只有时间。
#[test]
fn flush_slot_returns_immediately_when_the_slot_is_empty() {
    let _guard = serial();
    let started = std::time::Instant::now();
    autosave::flush_slot();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "空槽位的 flush_slot 花了 {elapsed:?}，说明它没有提前返回"
    );
}

/// 槽位非空但已经落定（无待写、无 worker）时，同样立刻返回。
///
/// 这是「编辑过、但自动保存已经写完」的常态：用户打了字，等了两秒，再关窗。
/// 此时若还去 `flush()`，会走一遍加锁 → 查 pending → 返回 `last_saved` 的流程，
/// 结果正确但没必要。
#[test]
fn flush_slot_returns_immediately_when_nothing_is_pending() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "OLD");

    edit(&root, "a.md", "SYNCED", None);
    block_on(note_flush(root, "a.md".to_owned(), false)).unwrap();

    let started = std::time::Instant::now();
    autosave::flush_slot();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "无待写内容时 flush_slot 花了 {elapsed:?}，说明它没有走提前返回"
    );
    assert_eq!(disk(&vault, "a.md"), "SYNCED");
}

/// 冲突时 `flush_slot()` 不能 panic，也不能覆盖对方的内容。
///
/// 关窗路径吞掉错误是刻意的（没有地方展示），但「吞掉」不等于「乱写」：
/// 乐观锁仍然生效，用户的字留在磁盘外的原因和平时一模一样。
#[test]
fn flush_slot_swallows_a_conflict_without_overwriting() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let opened = seed(&root, "a.md", "MINE");

    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();

    edit(&root, "a.md", "MINE-EDITED", Some(opened.mtime));
    autosave::flush_slot();

    assert_eq!(disk(&vault, "a.md"), "THEIRS", "冲突时绝不能覆盖外部内容");
}

/* ───────────────────── 槽位对齐（聚焦校验 / 重新加载） ───────────────────── */

/// 外部**原样重存**之后，`rebase` 把基线推到新 mtime，下一次保存不该再冲突。
///
/// 这条锁的是 `adopt-baseline` 的兑现：内容相同、mtime 变了，前端不该弹横幅，
/// 但基线必须前进。没有 `rebase` 的话，用户会看到「我什么都没改，它说我冲突了」——
/// 一个他完全无法理解的假冲突，而唯一的出口是「用我的版本覆盖」（覆盖的是
/// 他自己都没改过的内容）。
#[test]
fn rebase_adopts_the_new_mtime_so_the_next_save_does_not_conflict() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let opened = seed(&root, "a.md", "SAME");

    edit(&root, "a.md", "SAME", Some(opened.mtime));
    block_on(note_flush(root.clone(), "a.md".to_owned(), false)).unwrap();

    // 外部工具原样重存：内容不变、mtime 变。
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "SAME").unwrap();
    let reread = block_on(note_read(root.clone(), "a.md".to_owned())).unwrap();
    assert_ne!(
        reread.mtime, opened.mtime,
        "夹具没换 mtime：这条用例什么都没验"
    );

    assert!(
        autosave::rebase(Path::new(&root), "a.md", reread.mtime, false),
        "槽位就是这一篇、又没有待写内容，基线必须推得动"
    );

    edit(&root, "a.md", "SAME + 1", None);
    let saved = block_on(note_flush(root.clone(), "a.md".to_owned(), false))
        .expect("基线已经对齐，这次保存不该冲突");
    assert!(saved.is_some(), "应该有内容真的被写下去");
    assert_eq!(disk(&vault, "a.md"), "SAME + 1");
}

/// 槽位里还有**真正不同的**待写内容时，`rebase` 必须拒绝推基线。
///
/// 推了会怎样：那份内容带着新基线写下去，把别人刚保存的版本静默盖掉——
/// 而这正是整个 B-09 要消灭的事故。宁可让下一次保存照常冲突：
/// 冲突横幅是用户看得见、能选择的。
#[test]
fn rebase_refuses_to_advance_over_pending_content() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let opened = seed(&root, "a.md", "MINE");

    // 第一笔立刻落盘（首次编辑不节流），第二笔才会在 1.5 秒的节流窗口里
    // 压成 pending。只 edit 一次的话，worker 会在我们制造外部改动之前就写完了。
    edit(&root, "a.md", "MINE-1", Some(opened.mtime));
    block_on(note_flush(root.clone(), "a.md".to_owned(), false)).unwrap();
    edit(&root, "a.md", "MINE-2", None);

    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();
    let reread = block_on(note_read(root.clone(), "a.md".to_owned())).unwrap();

    assert!(
        !autosave::rebase(Path::new(&root), "a.md", reread.mtime, false),
        "有待写内容时不许推基线"
    );

    let err = block_on(note_flush(root.clone(), "a.md".to_owned(), false))
        .expect_err("基线没推，保存必须仍然冲突");
    assert!(matches!(err, KiError::Conflict { .. }), "实际 {err:?}");
    assert_eq!(disk(&vault, "a.md"), "THEIRS", "对方的内容一个字节都不能动");
}

/// `drop_pending = true`（用户点了「重新加载」）：槽位里那一版**一起丢**。
///
/// 只推基线、留着 pending 的话，1.5 秒后 worker 会拿新基线把旧内容写下去，
/// 正好盖掉用户刚刚同意接受的那一版——「重新加载」变成了「稍后覆盖」。
#[test]
fn rebase_with_drop_pending_discards_the_unsaved_content() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let opened = seed(&root, "a.md", "MINE");

    // 同 rebase_refuses_to_advance_over_pending_content：要两笔，
    // 第二笔才会真的压在槽位里。
    edit(&root, "a.md", "MINE-1", Some(opened.mtime));
    block_on(note_flush(root.clone(), "a.md".to_owned(), false)).unwrap();
    edit(&root, "a.md", "MINE-2", None);

    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(vault.path().join("a.md"), "THEIRS").unwrap();
    let reread = block_on(note_read(root.clone(), "a.md".to_owned())).unwrap();

    assert!(autosave::rebase(
        Path::new(&root),
        "a.md",
        reread.mtime,
        true
    ));

    let flushed = block_on(note_flush(root.clone(), "a.md".to_owned(), false)).unwrap();
    assert!(flushed.is_none(), "丢弃之后槽位里不该还有内容");
    assert_eq!(disk(&vault, "a.md"), "THEIRS", "重新加载之后磁盘不该被改写");

    // 对齐之后的正常编辑仍然要能存下去——否则「重新加载」等于把这篇弄坏了。
    edit(&root, "a.md", "THEIRS + 1", None);
    block_on(note_flush(root.clone(), "a.md".to_owned(), false)).unwrap();
    assert_eq!(disk(&vault, "a.md"), "THEIRS + 1");
}

/// 槽位停在别的文档上（或还是空的）时，`rebase` **接管**它。
///
/// 「打开 B 但一个字都没敲」时槽位的键还停在上一篇 A：切换只在 `submit` 里
/// 发生。此时不接管的话，用户敲下的第一个字会走「切换文档」分支，采信编辑器
/// **挂载时**捕获的旧 mtime——而外部改动刚换掉的正是它。
#[test]
fn rebase_takes_over_a_slot_that_still_points_at_another_note() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    seed(&root, "a.md", "A");
    let b = seed(&root, "b.md", "B");

    assert!(
        autosave::rebase(Path::new(&root), "b.md", b.mtime, false),
        "槽位空着、也没有在途写盘，应该接管"
    );
    // 接管之后 `note_status` 认这一篇，且报的就是新基线。
    let status = autosave::status(Path::new(&root), "b.md");
    assert_eq!(status.last_saved_mtime, Some(b.mtime));
    assert!(!status.dirty);

    // 第一笔编辑按同一篇处理（不再是「切换文档」），基线是刚对齐的那个。
    edit(&root, "b.md", "B + 1", None);
    let saved = block_on(note_flush(root.clone(), "b.md".to_owned(), false)).unwrap();
    assert!(saved.is_some());
    assert_eq!(disk(&vault, "b.md"), "B + 1");
}

/// 槽位里压着**另一篇**的字时，`rebase` 不许接管。
///
/// 接管等于把那篇的待写内容连键一起换掉：用户会看到「在 A 里敲的字，
/// 打开 B 之后没了」。宁可这次不推，让 A 的保存照常走它自己的路径。
#[test]
fn rebase_refuses_to_steal_another_documents_pending() {
    let _guard = serial();
    let vault = TempDir::new().unwrap();
    let root = root_of(&vault);
    let a = seed(&root, "a.md", "A");
    seed(&root, "b.md", "B");

    // 两笔：第一笔立刻落盘，第二笔压在 1.5 秒的节流窗口里。
    edit(&root, "a.md", "A-1", Some(a.mtime));
    block_on(note_flush(root.clone(), "a.md".to_owned(), false)).unwrap();
    edit(&root, "a.md", "A-2", None);

    assert!(
        !autosave::rebase(Path::new(&root), "b.md", a.mtime + 1, true),
        "槽位里压着 a.md 的字，不该被 b.md 接管"
    );
    // a.md 的待写内容还在，且仍然属于 a.md。
    let status = autosave::status(Path::new(&root), "a.md");
    assert!(status.dirty, "a.md 的待写内容被弄丢了");
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
