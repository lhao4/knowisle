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

/// 改名前的准备：如果槽位里正是这篇笔记，先把它彻底写完，再让调用方动磁盘。
///
/// # 为什么不能只在前端 flush
///
/// 前端 `note_flush` 返回时，Rust 侧的内容确实已经在磁盘上了——但它返回的
/// 是**那一刻**的槽位状态。worker 线程可能刚好被调度起来，手里握着一份
/// 更早取走的 pending；`flush` 会等它收工（见函数体的等待分支），
/// 但**调用方必须知道这件事已经做完了**才能去 rename，否则顺序就反了：
/// 文件先改名，worker 后落盘 → 内容写到已经不存在的旧路径上，旧文件复活。
///
/// 所以这条规则只能落在 Rust 侧：rename 命令先调本函数，成功后才碰目录项。
///
/// # 返回 `Err` 就**不要改名**
///
/// 典型是冲突：磁盘上那份是别人改的，我们的内容还在槽位里。此时改名等于
/// 把"写不出去的内容"搬到新路径上，用户看到的是"改名之后内容回退了"。
/// 让改名失败、把冲突横幅留在原地，是唯一不会骗人的选择。
///
/// # 改的不是当前文档
///
/// 直接放行。用户完全可能在树里右键改一篇**没打开**的笔记的名字，
/// 那与槽位无关；此时若去 flush，反而会把另一篇的内容写一遍。
pub fn drain_for_rename(root: &Path, path: &str) -> Result<(), KiError> {
    drain_if_current(root, path)
}

/// 删除前的准备：**与改名完全同一条契约**，只是理由不同。
///
/// 槽位里可能压着用户最后 1.5 秒的输入。直接 `rename` 进 trash，会让
/// "移到回收站"变成"丢掉最新版本"——文件在 trash 里，内容却是旧的，
/// 而且这件事没有任何提示。
///
/// 冲突（`Err`）时必须拒绝删除：此刻用户手里还有「覆盖 / 放弃」两个选择，
/// 删掉就只剩一个，而剩下的那个还不是他选的。
///
/// 单独起一个名字而不是让删除去调 `drain_for_rename`：两者的**判据**相同
/// （是不是当前文档），但**后果**不同（改名失败只是名字没变，删除失败是
/// 一条会丢数据的路径）。名字是调用方唯一会读的那行文档。
pub fn drain_for_delete(root: &Path, path: &str) -> Result<(), KiError> {
    drain_if_current(root, path)
}

/// 动目录项之前，把当前文档彻底写完。
///
/// `flush` 的契约是：返回 `Ok` 时 pending 必为空、worker 必已退出
/// （worker 还在写时会等，5 秒超时则返回 `Err`，那条路径由调用方挡掉）。
fn drain_if_current(root: &Path, path: &str) -> Result<(), KiError> {
    let is_current = {
        let slot = lock();
        slot.key.as_ref() == Some(&(root.to_path_buf(), path.to_owned()))
    };
    if !is_current {
        return Ok(());
    }

    flush(root, path, false).map(|_| ())
}

/// 改名成功之后，把槽位从旧路径搬到新路径。
///
/// # 为什么必须搬
///
/// 槽位以 `(root, path)` 为键。改名后若键还指着旧路径，下一次 `note_edit`
/// 会被判成"换了一篇笔记"：`drain_previous` 把新路径的内容写一遍，
/// 然后把槽位切到旧路径——worker 于是把内容写到**已经不存在的旧文件**上，
/// 树里看不到、搜索搜不到，而用户以为自己在编辑改名后的那一篇。
///
/// 只搬键、不重置基线：rename 不改 mtime（三平台一致），所以乐观锁的基线
/// 仍然有效；重置成 `None` 会让下一次自动保存退化成"无条件覆盖"。
pub fn rename_slot(root: &Path, old_path: &str, new_path: &str) {
    let mut slot = lock();
    if slot.key.as_ref() != Some(&(root.to_path_buf(), old_path.to_owned())) {
        // 改的不是当前文档：槽位里是另一篇，一个字都不该动。
        return;
    }
    slot.key = Some((root.to_path_buf(), new_path.to_owned()));

    // 理论上 `drain_for_rename` 已经把 pending 清空了，这里只是保险：
    // 万一将来有人在 rename 之前插了一条别的写入路径，也不至于写回旧路径。
    if let Some(job) = slot.pending.as_mut() {
        job.path = new_path.to_owned();
    }
    // 回执里的路径也要跟着改：`note_flush` 在没有待写内容时返回它，
    // 前端拿到的"磁盘现状"不该指着一个已经不存在的路径。
    if let Some(saved) = slot.last_saved.as_mut() {
        saved.path = new_path.to_owned();
    }
}

/// 转存为 UTF-8 之后，把槽位对齐到新的 mtime。
///
/// # 为什么必须有这一步
///
/// 转存改的是**磁盘上的字节**：`size` 变长、`mtime` 变了。而槽位里的基线
/// 还停在转存之前——下一次 `note_edit` 会拿旧基线去比，必然 `CONFLICT`。
/// 用户看到的是「转存之后保存坏了」，而实际上是一次过期的乐观锁。
///
/// 前端也会用返回值替换 `activeNote`（那份 `mtime` 是新的），但
/// `submit` **只在切换文档时采信 `base_mtime`**——同一篇的后续调用读的是
/// `slot.baseline`。所以这条对齐只能落在 Rust 侧。
///
/// # 三件事
///
/// 1. `baseline` 前进到新 mtime（乐观锁的基准是磁盘真值）。
/// 2. 槽位里若还压着待写内容（典型：GBK 里打的那个 emoji——它正是转存的
///    理由，而它当初存不进去），把它的 `expected_mtime` 一起换掉。不换的话
///    转存后的第一次重试仍然撞同一个冲突。
/// 3. 清掉 `last_error`：那条「无法按 GBK 保存」已经过期了，留着会让
///    「保存失败」的横幅在转存成功后还挂着。
///
/// # 不是当前文档时一个字都不动
///
/// 用户完全可能在树里对一篇**没打开**的笔记转存，此时槽位里是另一篇的内容。
pub fn rebase_after_transcode(root: &Path, path: &str, note: &ki_core::NoteContent) {
    let mut slot = lock();
    if slot.key.as_ref() != Some(&(root.to_path_buf(), path.to_owned())) {
        return;
    }

    slot.baseline = Some(note.mtime);
    slot.last_saved_mtime = Some(note.mtime);
    slot.last_saved = Some(SaveResult {
        path: note.path.clone(),
        size: note.size,
        mtime: note.mtime,
    });
    slot.last_error = None;
    if let Some(job) = slot.pending.as_mut() {
        job.expected_mtime = Some(note.mtime);
    }
}

/// 聚焦校验 / 重新加载之后，把槽位对齐到「当前打开的这一篇 + 磁盘真值」。
///
/// # 为什么前端做不到（这一条决定了它必须是一个 IPC）
///
/// [`submit`] 的 `base_mtime` **只在切换文档时采信一次**：同一篇的后续调用
/// 读的是 `slot.baseline`，而基线只在**成功落盘**时前进。而「外部把文件原样
/// 重写了一遍」「用户点了重新加载」这两种情形都是**没有落盘、磁盘却换了
/// mtime**——前端无论怎么传 `base_mtime` 都推不动基线，下一次保存必然
/// `CONFLICT`。那个冲突是假的，用户看到的是「我什么都没改，它说我冲突了」。
///
/// 与 [`rebase_after_transcode`] 是同一件事（都是「磁盘变了、槽位要跟上」），
/// 区别只在它不需要新字节，所以也不需要一份 `NoteContent`。
///
/// # 槽位停在别的文档上时**接管**它
///
/// 「打开 B 但一个字都没敲」时槽位的键还停在上一篇 A（切换只在
/// `submit` 里发生）。此时若不接管，用户敲下第一个字会走「切换文档」分支，
/// 采信编辑器**挂载时**捕获的旧 mtime——外部改动刚换掉的正是它。
/// 接管是安全的，但有两个前提：槽位里没有待写内容、也没有 worker 在跑。
/// 少判任何一个，接管都会把另一篇的字（或一次正在进行的写盘结果）算到
/// 这一篇头上。
///
/// # `drop_pending` 的两种语义
///
/// - `true`：用户点了「重新加载」，等于明确丢弃自己那一版。压着的内容必须
///   **一起丢**——只推基线、留着 pending 的话，1.5 秒后 worker 会拿新基线
///   把旧内容写下去，正好盖掉他刚刚同意接受的那一版。
/// - `false`：只推基线（内容相同、mtime 变了）。此时若槽位里还有待写内容，
///   **什么都不做**：那份内容会带着新基线写下去，把别人的版本盖掉。
///   宁可不推，让下一次保存照常冲突——冲突横幅至少是用户看得见、能选择的。
///
/// # 返回值
///
/// `false` = 槽位里压着**另一篇**的待写内容（或正有一篇在写），不能接管；
/// 或者 `drop_pending = false` 而这一篇自己还有真正不同的待写内容。
/// 两种都不需要调用方补救。
///
/// # 一个已知的窄竞态
///
/// worker 已经取走作业、正在 `write_note_checked` 里时，它的失败会把
/// `pending` 与 `last_error` 放回槽位，覆盖本函数刚清掉的状态。窗口是
/// 一次写盘的时长（毫秒级），后果只是横幅多挂一会儿——下一次 `submit`
/// 会用新基线覆盖 pending，内容不会丢。为它加代际计数不划算。
pub fn rebase(root: &Path, path: &str, mtime: i64, drop_pending: bool) -> bool {
    let target = (root.to_path_buf(), path.to_owned());
    let mut slot = lock();

    if slot.key.as_ref() != Some(&target) {
        // 接管空槽位（或停在别处、且那篇没有任何在途工作的槽位）。
        if slot.pending.is_some() || slot.worker_running {
            return false;
        }
        slot.key = Some(target);
        slot.throttle.reset();
    } else if drop_pending {
        slot.pending = None;
    } else if slot.pending.is_some() {
        return false;
    }

    slot.baseline = Some(mtime);
    slot.last_saved_mtime = Some(mtime);
    // 回执里的 size 与 mtime 都过期了，而 `note_flush` 在没有待写内容时
    // 会把它当成「磁盘现状」交出去。清掉比交一份假的强。
    slot.last_saved = None;
    // 上一次的冲突/编码错误已经随着这次对齐过期了，留着会让横幅挂在一个
    // 不存在的分歧上。
    slot.last_error = None;
    true
}

/// 删除成功之后，把槽位**清空**（不是搬到别的键上）。
///
/// # 这是整条删除路径上唯一防「文件复活」的硬防线
///
/// 前端也会丢本地缓冲（`NoteEditorHandle.discardPending`），但那只挡得住
/// 300ms 合并窗口里的那一次 `noteEdit`。挡不住的是**已经在飞的 IPC**：
/// `flushActiveNote` 的 `noteFlush` 可能正走在路上，等它到达时文件已经进了
/// trash。它落在已删除路径上会写回一个新文件——删除看起来"没生效"。
///
/// 清掉 `key` 之后，`flush` 的第一道判断就是 `slot.key != key → Ok(None)`，
/// 于是任何迟到的 flush 都是无操作。**这条不许省**，它比前端那层更靠后，
/// 也更难绕过。
///
/// # 为什么不是搬键
///
/// 改名可以把键从旧路径搬到新路径（文件还在，只是换了名字）。删除没有新路径，
/// 搬键只能搬到一个假名字上——那等于给"下一次编辑"留了个会写脏的坑。
///
/// # 删的不是当前文档
///
/// 一个字都不动。用户完全可能在树里删一篇**没打开**的笔记，此时槽位里是
/// 另一篇的内容，清掉就是把它最后几个字扔了。
pub fn clear_for_delete(root: &Path, path: &str) {
    let mut slot = lock();
    if slot.key.as_ref() != Some(&(root.to_path_buf(), path.to_owned())) {
        return;
    }

    // 到这一步 `drain_for_delete` 已经跑过了：pending 为空、worker 已退出，
    // 所以这里不需要（也不该）碰 `worker_running`——把一个正在写的线程标成
    // "没在跑"会让后来者同时起第二个 worker。
    slot.key = None;
    slot.pending = None;
    slot.baseline = None;
    slot.last_saved_mtime = None;
    slot.last_saved = None;
    slot.last_error = None;
}

/// 冲刷**当前槽位**，供关窗路径调用。
///
/// # 为什么无参
///
/// 关窗那一刻没有任何人知道「现在打开的是哪一篇」——前端还在跑，但调用栈里
/// 只有窗口事件。槽位自己记着 `(root, path)`，所以让它自己答。多传两个参数
/// 只会多一个「传错了就写到别的文件」的机会。
///
/// # 为什么必须是同步函数
///
/// `CloseRequested` 的回调不能 `await`：放行窗口的语句就在回调返回之后。
/// 返回一个 `Future` 等于要么忘了等（内容没落盘就退出），要么在同步回调里
/// 阻塞等待（那就是死锁）。所以这里一路同步：加锁、取作业、写盘、返回。
///
/// # 槽位为空时**不碰磁盘**
///
/// 「没编辑过」和「编辑过但已经落盘」都不该在关窗路径上多一次 `stat`。
/// 关窗是用户已经失去耐心之后才按的按钮，这里每多一毫秒都是可见的卡顿。
///
/// # 错误被吞掉
///
/// 窗口正在关闭，没有地方展示错误，也不该为了报错把关闭卡住。
/// 写失败时内容仍在槽位里，但进程即将退出——这是 M1 明确接受的代价
/// （见 `src-tauri/src/lib.rs` 里 `on_window_event` 的注释）。
pub fn flush_slot() {
    let key = {
        let slot = lock();
        match &slot.key {
            // 有槽位、且有东西要写（或已经有人在写）才值得往下走。
            Some(key) if slot.pending.is_some() || slot.worker_running => key.clone(),
            _ => return,
        }
    };
    // 锁已释放：`flush` 自己会重新加锁，持锁调它会直接死锁。
    let _ = flush(&key.0, &key.1, false);
}

/// 定时线程：等到 deadline，取走内容，落盘，记账，然后决定是否继续。
///
/// 线程是**按需起、干完就退**的：没有常驻线程，也就没有"应用退出时要通知它收工"
/// 的问题。用户打字期间每 1.5 秒创建一个线程，成本远低于一次 fsync。
fn worker() {
    loop {
        let deadline = {
            let mut slot = lock();
            if slot.pending.is_none() {
                // **必须与 `pending.is_none()` 在同一次持锁里放下 `worker_running`。**
                // 分两次（先判 pending、再调 `retire()`）会开一个窗口：
                // 提交恰好落在中间，它看到 `worker_running == true` 就不起线程，
                // 紧接着这里把标志放下——于是既没有线程在跑、内容也没人写，
                // 要等用户下一次按键才被救回来。
                slot.worker_running = false;
                drop(slot);
                SHARED.wake.notify_all();
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
