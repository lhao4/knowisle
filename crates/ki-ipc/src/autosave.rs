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
    /// 最近一次成功落盘的回执。`note_flush` 在没有待写内容时返回它，
    /// 这样调用方总能拿到"磁盘上现在是什么"而不是一句"没东西可写"。
    last_saved: Option<SaveResult>,
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
            last_saved: None,
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
        self.last_saved = Some(saved.clone());
        self.baseline = Some(saved.mtime);
        self.last_error = None;
        self.throttle.mark_flushed(now);
    }

    /// 写一次并记账。
    ///
    /// `force` 为真时跳过乐观锁（用户在冲突提示里选了「覆盖」）。
    /// 冲突时把内容放回槽位——用户的字不能就这么丢了。
    fn write_job(&mut self, job: Job, force: bool) -> Result<SaveResult, KiError> {
        let expected = if force { None } else { job.expected_mtime };
        let result = write_note_checked(&job.root, &job.path, &job.content, expected);
        match &result {
            Ok(saved) => self.record_success(saved, Instant::now()),
            Err(err) => {
                self.pending = Some(job);
                self.last_error = Some(err.clone());
            }
        }
        result
    }
}

struct Shared {
    slot: Mutex<Slot>,
    /// worker 退出或完成一次落盘时唤醒等待者（`note_flush` / 切换文档）。
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

/// 等 worker 收工的上限。超过就放弃等待，宁可让调用方慢一点也不要永久卡住。
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 单次等待的切片。用短切片轮询而不是一次等满 5 秒：
/// `notify_all` 与 `wait` 之间存在丢失唤醒的窗口，短切片让重查足够及时。
const JOIN_POLL: Duration = Duration::from_millis(200);

/// 把上一篇彻底写完（或确认它已经写完）再切换。
///
/// # 为什么不能只 `take` 一次就切
///
/// worker 可能**已经**把 pending 取走、正在写。此时 `take` 拿到 `None`，
/// 若直接切换，槽位就指向了新文档，而 worker 还在写旧文档——它写完之后
/// 会把"成功"记到新文档头上，而旧文档的内容是否落盘完全没人知道。
/// 这条竞态在 Windows 上几乎撞不到（写盘慢，`take` 总能先到），
/// 在 macOS 上第一轮 CI 就撞到了两次。
///
/// # 返回值
///
/// 放弃等待时（磁盘卡死超过 5 秒）不报错，只是不再等——
/// 切换文档这件事本身不能被一次写盘卡住。
fn drain_previous() {
    let deadline = Instant::now() + JOIN_TIMEOUT;
    loop {
        // 上限放在循环顶部：下面的 `continue` 会跳过它，而那条路径
        // 恰好是唯一可能不收敛的地方（见写失败分支）。
        if Instant::now() > deadline {
            return;
        }

        let job = {
            let mut slot = lock();
            if let Some(job) = slot.pending.take() {
                Some(job)
            } else if slot.worker_running {
                None
            } else {
                return;
            }
        };

        if let Some(job) = job {
            let outcome = lock().write_job(job, false);
            SHARED.wake.notify_all();
            if outcome.is_err() {
                // **不能重试。** `write_job` 失败时会把 job 放回槽位，
                // 若这里 `continue`，下一轮就会把它原样取出来再写一次，
                // 同样的内容配同样的基线必然得到同样的错误——死循环。
                // Windows CI 上 `Test (ki-ipc)` 就是这么挂住的：
                // 前一个用例留下的过期基线让切换时的写盘必然冲突。
                //
                // 内容仍留在槽位里，等用户下次编辑或显式 flush 再试；
                // 旧文档写失败也不该拦住切换。
                return;
            }
            continue;
        }

        let slot = lock();
        let (guard, _) = SHARED
            .wake
            .wait_timeout(slot, JOIN_POLL)
            .unwrap_or_else(PoisonError::into_inner);
        drop(guard);
    }
}

/// 提交一次编辑：入队、按需起定时线程，返回当前保存状态。
///
/// `base_mtime` 是前端打开这篇笔记时读到的 mtime，**只在切换文档时采信一次**。
/// 同一文档的后续调用忽略它：基线在 Rust 侧随每次成功落盘向前推进。
pub fn submit(root: PathBuf, path: String, content: String, base_mtime: Option<i64>) -> SaveState {
    let target = (root, path);
    let switching = {
        let slot = lock();
        slot.key.as_ref() != Some(&target)
    };

    if switching {
        // 这一步在锁外做：它要写盘，持锁写盘会让 worker 和 flush 全部堵住。
        drain_previous();
    }

    let mut slot = lock();

    if slot.key.as_ref() != Some(&target) {
        slot.key = Some(target.clone());
        slot.baseline = base_mtime;
        slot.throttle.reset();
        // 新文档还没落过盘，也不该继承上一篇的状态与错误。
        slot.last_saved_mtime = None;
        slot.last_saved = None;
        slot.last_error = None;
    }

    slot.pending = Some(Job {
        root: target.0,
        path: target.1,
        content,
        expected_mtime: slot.baseline,
    });

    if slot.worker_running {
        return slot.state();
    }

    // 起一个新线程：它会先算 deadline 再决定睡多久。
    slot.worker_running = true;
    drop(slot);
    std::thread::spawn(worker);

    lock().state()
}

/// 立刻落盘。
///
/// # 契约
///
/// 调用返回后，**调用方提交过的内容一定已经在磁盘上**（除非返回 `Err`）。
///
/// - `Ok(Some(result))`：磁盘上是这份内容，`result` 是它的 stat。
///   可能是这次刚写的，也可能是 worker 刚写完的——对调用方没有区别。
/// - `Ok(None)`：这篇笔记本次会话从未写过任何东西。
/// - `Err(_)`：写失败，**内容仍留在槽位里**，下次编辑会重试。
///
/// `force` 为真时跳过乐观锁（用户在冲突提示里选了「覆盖」）。
pub fn flush(root: &Path, path: &str, force: bool) -> Result<Option<SaveResult>, KiError> {
    let key = (root.to_path_buf(), path.to_owned());
    let deadline = Instant::now() + JOIN_TIMEOUT;

    loop {
        let job = {
            let mut slot = lock();

            if slot.key.as_ref() != Some(&key) {
                // 不是当前文档：没有可落盘的内容。
                return Ok(None);
            }

            match slot.pending.take() {
                Some(job) => Some(job),
                None if slot.worker_running => None,
                None => {
                    // 没有待写内容：把上次的错误交出去。
                    // 用 `clone` 而不是 `take`——错误要一直留着，直到下一次成功落盘。
                    // 否则前端调一次 flush 就再也拿不到它，冲突提示会凭空消失。
                    return match slot.last_error.clone() {
                        Some(err) => Err(err),
                        None => Ok(slot.last_saved.clone()),
                    };
                }
            }
        };

        if let Some(job) = job {
            let result = lock().write_job(job, force);
            SHARED.wake.notify_all();
            return result.map(Some);
        }

        if Instant::now() > deadline {
            return Err(KiError::Internal("等待自动保存完成超时（5 秒）".to_owned()));
        }
        // worker 正在写：等它收工，然后重查。
        let slot = lock();
        let (guard, _) = SHARED
            .wake
            .wait_timeout(slot, JOIN_POLL)
            .unwrap_or_else(PoisonError::into_inner);
        drop(guard);
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
    // 先让上一篇落定，再清槽位。
    //
    // 不清就直接 `*slot = Slot::default()` 的话，上一个用例的 worker 可能
    // 还在写盘：它写完会把**旧文档的 mtime** 记进新槽位（`record_success`
    // 会推进 `baseline`），于是下一个用例的每一次写盘都拿一个过期的基线去比，
    // 全部报冲突。这条污染在 Windows CI 上表现为 `drain_previous` 死循环。
    drain_previous();
    let mut slot = lock();
    *slot = Slot::default();
}

/// 内部状态探针，只给测试用。
///
/// 返回 `(是否还有待落盘内容, 是否有线程在跑)`。
/// 测试需要它来**确定性地**命中"worker 已取走内容、正在写盘"这个窗口——
/// 靠 sleep 猜时间只能得到概率测试，变异验证时会漏掉目标分支。
#[doc(hidden)]
pub fn debug_state_for_test() -> (bool, bool) {
    let slot = lock();
    (slot.pending.is_some(), slot.worker_running)
}
