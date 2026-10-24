//! 自动保存的执行器。
//!
//! [`ki_vault::SaveThrottle`] 只回答「什么时候该落盘」，本模块负责
//! 「谁在等、谁在写、写失败了怎么办」。三件事都围绕一个共享槽位：
//!
//! | 职责 | 为什么必须在这里 |
//! |---|---|
//! | 持有待落盘内容 | 前端可以随手丢弃状态（刷新、崩溃），Rust 侧不能 |
//! | 起定时线程 | **定时器不能放在前端**——见下 |
//! | 串行化落盘 | worker 与 `note_flush` 可能同时想写同一个文件 |
//!
//! # 定时器为什么必须在 Rust 侧
//!
//! webview 的 `setTimeout` 会被浏览器引擎节流：窗口失焦或最小化时，
//! Chromium 把后台定时器压到约 **1 次/分钟**。用户切到别的应用去查资料，
//! 回来发现「自动保存」早就停摆了——这是自动保存类功能最典型的隐形失效，
//! 而且测试时很难发现（人盯着窗口的时候它一切正常）。
//!
//! 放在 Rust 侧的线程里，节流不存在，1.5 秒就是 1.5 秒。
//!
//! # 单槽位
//!
//! 只保存**当前正在编辑的那一篇**。M1 只有一个窗口、一个编辑器，
//! 多槽位是没有需求支撑的复杂度。切换笔记时旧文档会被同步落盘，
//! 代价是旧文档若落盘失败，错误会被新文档的状态覆盖——M1 接受这个限制，
//! 因为失败的原因（冲突）在切回来时会再次出现。

use std::path::{Path, PathBuf};
use std::sync::{Condvar, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ki_core::{KiError, SaveResult, SaveState};
use ki_vault::{write_note_checked, SaveThrottle};

/// 一次待落盘的作业。内容与路径都在这里，所以落盘时不必回查任何外部状态。
struct Job {
    root: PathBuf,
    path: String,
    content: String,
    /// 本次写入期望的磁盘 mtime。`None` 表示无条件覆盖。
    expected_mtime: Option<i64>,
}

struct Slot {
    /// 当前文档。用来判断「这次编辑是不是换了笔记」。
    key: Option<(PathBuf, String)>,
    /// 还没落盘的内容。`None` 表示磁盘与内存一致。
    pending: Option<Job>,
    /// 乐观锁基线：上一次成功落盘后的 mtime。换文档时由前端传入的值初始化。
    ///
    /// 必须由 Rust 维护，不能让前端每次带上来——自动落盘是异步的，
    /// 前端在 `note_edit` 返回时根本不知道新 mtime 是多少。
    baseline: Option<i64>,
    throttle: SaveThrottle,
    /// 是否已有一个线程在等或正在写。
    worker_running: bool,
    last_saved_mtime: Option<i64>,
    last_error: Option<KiError>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            key: None,
            pending: None,
            baseline: None,
            throttle: SaveThrottle::new(),
            worker_running: false,
            last_saved_mtime: None,
            last_error: None,
        }
    }
}

impl Slot {
    fn state(&self) -> SaveState {
        SaveState {
            dirty: self.pending.is_some(),
            last_saved_mtime: self.last_saved_mtime,
            last_error: self.last_error.clone(),
        }
    }

    /// 落盘成功后的统一记账。`throttle` 只在这里推进——
    /// 失败时**不**推进，否则错误会被静默推迟 1.5 秒。
    fn record_success(&mut self, saved: &SaveResult, now: Instant) {
        self.last_saved_mtime = Some(saved.mtime);
        self.baseline = Some(saved.mtime);
        self.last_error = None;
        self.throttle.mark_flushed(now);
    }
}

struct Shared {
    slot: Mutex<Slot>,
    /// worker 退出或完成一次落盘时唤醒等待者（`note_flush`）。
    wake: Condvar,
}

static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    slot: Mutex::new(Slot::default()),
    wake: Condvar::new(),
});

fn lock() -> std::sync::MutexGuard<'static, Slot> {
    // 槽位里只有字符串与枚举，任何线程 panic 都不会让数据处于"半更新"状态。
    // 因此中毒后继续用比 panic 传播更合理——自动保存挂了会丢数据。
    SHARED.slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 提交一次编辑：入队、按需起定时线程，返回当前保存状态。
///
/// `base_mtime` 是前端打开这篇笔记时读到的 mtime，**只在切换文档时采信一次**。
/// 同一文档的后续调用忽略它：基线在 Rust 侧随每次成功落盘向前推进。
pub fn submit(root: PathBuf, path: String, content: String, base_mtime: Option<i64>) -> SaveState {
    let mut slot = lock();

    let switching = slot.key.as_ref() != Some(&(root.clone(), path.clone()));
    if switching {
        // 旧文档还有没落盘的内容：同步写完再切。宁可这一次 IPC 慢 10ms，
        // 也不要让"切走"变成一次静默的数据丢失。
        if let Some(job) = slot.pending.take() {
            let result = write_note_checked(&job.root, &job.path, &job.content, job.expected_mtime);
            match result {
                Ok(saved) => slot.record_success(&saved, Instant::now()),
                Err(err) => slot.last_error = Some(err),
            }
        }
        slot.key = Some((root.clone(), path.clone()));
        slot.baseline = base_mtime;
        slot.throttle.reset();
        slot.last_saved_mtime = None;
    }

    slot.pending = Some(Job {
        root,
        path,
        content,
        expected_mtime: slot.baseline,
    });

    if slot.worker_running {
        return slot.state();
    }

    // 到期了才起线程；没到期说明已有一个 worker 在等（但 worker_running 为 false
    // 表示它刚退出），这时直接起新的即可——新的会先算 deadline 再睡。
    slot.worker_running = true;
    drop(slot);
    std::thread::spawn(worker);

    lock().state()
}

/// 立刻落盘，返回落盘结果。
///
/// - `Ok(None)`：没有待落盘内容（已是最新）。
/// - `Ok(Some(_))`：刚刚写成功。
/// - `Err(_)`：写失败，**内容仍留在槽位里**，下次编辑会重试。
///
/// `force` 为真时跳过乐观锁（用户在冲突提示里选了「覆盖」）。
pub fn flush(root: &Path, path: &str, force: bool) -> Result<Option<SaveResult>, KiError> {
    let key = (root.to_path_buf(), path.to_owned());

    loop {
        let job = {
            let mut slot = lock();

            if slot.key.as_ref() != Some(&key) {
                // 不是当前文档：没有可落盘的内容。
                // 但要把"这篇曾经写失败"的事实报出去，否则错误永远不会被看见。
                return Ok(None);
            }

            match slot.pending.take() {
                Some(job) => job,
                None if slot.worker_running => {
                    // worker 正在写，等它把 timer_running 放下来。
                    // 超时兜底：worker 若因为磁盘卡死而长时间不返回，
                    // 调用方至少要拿到一个"没写完"的答案而不是永久阻塞。
                    let (guard, timeout) = SHARED
                        .wake
                        .wait_timeout(slot, Duration::from_secs(5))
                        .unwrap_or_else(PoisonError::into_inner);
                    drop(guard);
                    if timeout.timed_out() {
                        return Err(KiError::Internal("等待自动保存完成超时（5 秒）".to_owned()));
                    }
                    continue;
                }
                None => {
                    // 没有待写内容：把上次的错误交出去。
                    // 用 `clone` 而不是 `take`——错误要一直留着，直到下一次成功落盘。
                    // 否则前端调一次 flush 就再也拿不到它，冲突提示会凭空消失。
                    return match slot.last_error.clone() {
                        Some(err) => Err(err),
                        None => Ok(None),
                    };
                }
            }
        };

        let expected = if force { None } else { job.expected_mtime };
        let result = write_note_checked(&job.root, &job.path, &job.content, expected);

        let mut slot = lock();
        match &result {
            Ok(saved) => slot.record_success(saved, Instant::now()),
            Err(err) => {
                // 内容放回槽位：用户的字还在编辑器里，但也不能就这么丢了。
                // 不推进 throttle，所以下一次编辑会立刻重试。
                slot.pending = Some(job);
                slot.last_error = Some(err.clone());
            }
        }
        SHARED.wake.notify_all();

        return result.map(Some);
    }
}

/// 定时线程：等到 deadline，取走内容，落盘，记账，然后决定是否继续。
///
/// 线程是**按需起、干完就退**的：没有常驻线程，也就没有"应用退出时要通知它收工"
/// 的问题。用户打字期间每 1.5 秒创建一个线程，成本远低于一次 fsync。
fn worker() {
    loop {
        let deadline = {
            let slot = lock();
            if slot.pending.is_none() {
                drop(slot);
                retire();
                return;
            }
            slot.throttle.deadline(Instant::now())
        };

        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        }

        let job = {
            let mut slot = lock();
            match slot.pending.take() {
                Some(job) => job,
                None => {
                    slot.worker_running = false;
                    drop(slot);
                    SHARED.wake.notify_all();
                    return;
                }
            }
        };

        let result = write_note_checked(&job.root, &job.path, &job.content, job.expected_mtime);
        let now = Instant::now();

        let mut slot = lock();
        match result {
            Ok(saved) => slot.record_success(&saved, now),
            Err(err) => {
                // 冲突（或任何写失败）：内容留在槽位里，但不在这里重试——
                // 立刻重试只会每 1.5 秒刷同一个错误。等用户下一次输入再触发，
                // 那时前端已经看到了错误提示。
                slot.pending = Some(job);
                slot.last_error = Some(err);
                slot.worker_running = false;
                drop(slot);
                SHARED.wake.notify_all();
                return;
            }
        }
        SHARED.wake.notify_all();
    }
}

/// 把 `worker_running` 放下来并唤醒可能在等的 `note_flush`。
fn retire() {
    let mut slot = lock();
    slot.worker_running = false;
    drop(slot);
    SHARED.wake.notify_all();
}

/// 查询当前保存状态，不触发任何写入。
///
/// 前端在 `dirty` 为真时用它轮询——自动落盘发生在 Rust 侧，
/// 前端没有别的办法知道它什么时候完成。空闲时不轮询，所以开销只落在
/// 「刚打完字、还没落盘」的那一两秒里。
pub fn status(root: &Path, path: &str) -> SaveState {
    let slot = lock();
    if slot.key.as_ref() != Some(&(root.to_path_buf(), path.to_owned())) {
        // 问的不是当前文档：没有待落盘内容可言。
        return SaveState {
            dirty: false,
            last_saved_mtime: None,
            last_error: None,
        };
    }
    slot.state()
}

/// 把槽位恢复到初始状态。
///
/// 只给测试用：`cargo test` 的多个用例共享同一个进程级静态变量，
/// 不重置会互相污染。
///
/// 无条件编译而不是 `#[cfg(test)]`——集成测试（`tests/`）链接的是普通 lib，
/// 看不到只在单元测试下存在的符号。
#[doc(hidden)]
pub fn reset_for_test() {
    let mut slot = lock();
    *slot = Slot::default();
}
