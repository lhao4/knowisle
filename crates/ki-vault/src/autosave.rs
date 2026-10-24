//! 自动保存的**时机决策**。
//!
//! 这里只有纯逻辑：给定「上一次落盘的时刻」和「现在」，算出下一次最晚该在
//! 什么时候落盘。线程、锁、真正的写盘都在 `ki-ipc`——把时机策略隔离出来，
//! 它就能注入时钟做确定性测试，不必和线程调度纠缠。
//!
//! # 为什么是 throttle 而不是 debounce
//!
//! 这是「断电最多丢 1.5 秒」能不能成立的全部原因。
//!
//! | 策略 | 用户连续输入 10 分钟 | 断电最坏丢失 |
//! |---|---|---|
//! | debounce 1.5s | 计时器被无限重置，**一次都没落盘** | 全部 |
//! | throttle 1.5s | 每 1.5 秒强制落一次 | **≤ 1.5 秒** |
//!
//! debounce 的语义是「安静下来之后再做」，它给不出上界——只要用户不停手，
//! 它就永远不做。用它做自动保存是本末倒置：用户以为存了，实际一个字都没落盘。
//!
//! throttle 的语义是「每隔 1.5 秒做一次」，天然有上界。代价是连续输入时
//! 会多写几次盘（1.5 秒一次，对 SSD 而言可以忽略）。
//!
//! # 时序
//!
//! ```text
//! 编辑 e1    e2      e3         e4   e5
//!   │        │       │          │    │
//!   ▼        ▼       ▼          ▼    ▼
//! ──┬────────┬───────┬──────────┬────┬──────→
//!   │        │       │          │    │
//!   ▼        ▼       ▼          ▼    ▼
//! 落盘     (静默)   落盘       (静默) 落盘
//! ```
//!
//! 每次落盘取「此刻的最新内容」，所以中间那些编辑不会各写一遍盘。

use std::time::{Duration, Instant};

/// 自动保存间隔。
///
/// 这个数字就是 M1 对用户的承诺：**断电最多丢 1.5 秒的输入**。
/// 改大它等于直接改写承诺，改小它会让 fsync 频率追上打字频率。
pub const AUTOSAVE_INTERVAL: Duration = Duration::from_millis(1500);

/// 落盘节流器。
///
/// 只记一件事：上一次落盘是什么时候。所有判断都从它推导，
/// 因此整个结构体可以被复制、被断言、被注入任意时间点。
#[derive(Debug, Clone)]
pub struct SaveThrottle {
    interval: Duration,
    /// `None` 表示本次会话还没有落过盘。
    last_flush: Option<Instant>,
}

impl Default for SaveThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl SaveThrottle {
    pub fn new() -> Self {
        Self::with_interval(AUTOSAVE_INTERVAL)
    }

    /// 自定义间隔。只给测试用——生产代码永远走 [`SaveThrottle::new`]，
    /// 否则「1.5 秒」这个承诺会散落在各处。
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            last_flush: None,
        }
    }

    /// 下一次落盘最晚该在什么时刻发生。
    ///
    /// 从未落过盘时返回 `now`——首次编辑立刻落盘，而不是先等 1.5 秒。
    /// 这样"打开笔记改一个字就关掉"不会产生一个 1.5 秒的空窗，
    /// 而且让"新文档的第一笔"语义上更接近用户的直觉。
    pub fn deadline(&self, now: Instant) -> Instant {
        match self.last_flush {
            None => now,
            Some(last) => last + self.interval,
        }
    }

    /// 现在是否已经到点。
    pub fn is_due(&self, now: Instant) -> bool {
        self.deadline(now) <= now
    }

    /// 记录一次成功的落盘。
    ///
    /// **只有真正写成功才调用**。写失败（比如冲突）时若也调用，
    /// 节流器会以为数据已经在磁盘上，于是把下一次尝试推迟 1.5 秒——
    /// 而用户看到的错误提示与实际落盘状态就对不上了。
    pub fn mark_flushed(&mut self, now: Instant) {
        self.last_flush = Some(now);
    }

    /// 距上一次落盘过了多久。用于诊断与测试断言。
    pub fn age(&self, now: Instant) -> Option<Duration> {
        self.last_flush
            .map(|last| now.saturating_duration_since(last))
    }

    /// 文档被切换时重置：新文档有自己的落盘节奏。
    pub fn reset(&mut self) {
        self.last_flush = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 首次编辑立刻到期：不等 1.5 秒。
    #[test]
    fn first_edit_is_due_immediately() {
        let throttle = SaveThrottle::new();
        let now = Instant::now();
        assert!(throttle.is_due(now), "第一次编辑应当立刻落盘");
        assert_eq!(throttle.deadline(now), now);
    }

    /// 刚落完盘，立刻编辑不该再写一次——否则每次按键都是一次 fsync。
    #[test]
    fn edit_right_after_flush_waits_for_the_interval() {
        let mut throttle = SaveThrottle::new();
        let t0 = Instant::now();
        throttle.mark_flushed(t0);

        let t1 = t0 + Duration::from_millis(100);
        assert!(!throttle.is_due(t1), "刚落盘 100ms，不该再次落盘");
        assert_eq!(
            throttle.deadline(t1),
            t0 + AUTOSAVE_INTERVAL,
            "下一次必须落在 t0 + 1.5s"
        );

        let t2 = t0 + AUTOSAVE_INTERVAL;
        assert!(throttle.is_due(t2), "满 1.5 秒必须到期");
    }

    /// **核心用例**：连续输入 10 秒，落盘次数与最大滞后都要满足承诺。
    ///
    /// 这条测试就是「断电最多丢 1.5 秒」的可执行形式。
    /// 换成 debounce 实现时，`flushes` 会是 1，`max_age` 会是 10 秒。
    #[test]
    fn continuous_typing_never_lags_more_than_the_interval() {
        let mut throttle = SaveThrottle::new();
        let base = Instant::now();

        let mut flushes = 0u32;
        let mut max_age = Duration::ZERO;

        // 每 10ms 一次按键，持续 10 秒。
        for step in 0..1000u64 {
            let now = base + Duration::from_millis(step * 10);

            if throttle.is_due(now) {
                throttle.mark_flushed(now);
                flushes += 1;
            }

            if let Some(age) = throttle.age(now) {
                max_age = max_age.max(age);
            }
        }

        // 10 秒 / 1.5 秒 = 6.67，至少 6 次。
        assert!(
            flushes >= 6,
            "连续输入 10 秒只落盘 {flushes} 次，节流器没有起到强制落盘的作用"
        );
        // 允许一次 tick 的粒度误差（10ms），不允许超出 1.5 秒。
        assert!(
            max_age <= AUTOSAVE_INTERVAL + Duration::from_millis(10),
            "最大滞后 {max_age:?} 超过承诺的 1.5 秒"
        );
    }

    /// 空闲很久之后的一次编辑：立刻落盘，而不是等下一个周期。
    #[test]
    fn edit_after_a_long_idle_flushes_immediately() {
        let mut throttle = SaveThrottle::new();
        let t0 = Instant::now();
        throttle.mark_flushed(t0);

        let later = t0 + Duration::from_secs(3600);
        assert!(
            throttle.is_due(later),
            "空闲一小时后编辑必须立刻落盘，不该再等 1.5 秒"
        );
    }

    /// 落盘失败时**不能**调用 `mark_flushed`——这里锁定的是调用方的义务，
    /// 因为本结构体无法分辨成功与失败。
    #[test]
    fn a_failed_flush_must_not_postpone_the_next_attempt() {
        let mut throttle = SaveThrottle::new();
        let t0 = Instant::now();
        throttle.mark_flushed(t0);

        // 到点后写盘失败：不调用 mark_flushed。
        let t1 = t0 + AUTOSAVE_INTERVAL;
        assert!(throttle.is_due(t1));

        // 10ms 后重试仍然是"到期"状态，不会被推迟。
        let t2 = t1 + Duration::from_millis(10);
        assert!(
            throttle.is_due(t2),
            "失败后必须保持到期状态，否则错误会被静默推迟 1.5 秒"
        );
    }

    /// 切换文档后重置，新文档的第一笔立刻落盘。
    #[test]
    fn reset_makes_the_next_edit_due() {
        let mut throttle = SaveThrottle::new();
        let t0 = Instant::now();
        throttle.mark_flushed(t0);
        throttle.reset();

        assert!(throttle.is_due(t0 + Duration::from_millis(1)));
        assert_eq!(throttle.age(t0), None);
    }
}
