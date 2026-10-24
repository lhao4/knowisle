//! 回答 M1 的那个问题：**用户现在断电，丢多少？**
//!
//! `tests/crash.rs`（在 `ki-vault`）验的是**原子性**：崩溃后不许出现半截内容。
//! 本文件验的是**丢失量**：崩溃后最多丢多久的输入。两者是不同的问题——
//! 一个文件可以每字节都完整，但仍然落后于用户手指 10 分钟。
//!
//! # 怎么量
//!
//! 子进程以固定节拍（10ms）往同一个字符串尾部加一个字，每加一个就
//! `autosave::submit` 一次（走真实的节流器与定时线程）。它同时**盯着磁盘**：
//! 文件长度一变就打印 `FLUSHED <字节数> <已耗时ms>`，每轮再打印
//! `TYPED <字符数> <已耗时ms>`。
//!
//! 父进程等**第一次观测到落盘**（锚点，避开进程冷启动的偏斜），再随机等
//! 0–1999ms 后硬杀，然后：
//!
//! ```text
//! 磁盘落后 = 被杀时刻的 elapsed − 最后一次观测到落盘的 elapsed
//! ```
//!
//! 判据：**P95 ≤ 1500ms + 500ms 调度抖动预算**。
//!
//! # 为什么直接量磁盘，而不是「丢的字符数 × 平均节拍」
//!
//! 后者要先把字符数换算成毫秒，换算系数是子进程的实测打字速率。
//! CI runner 上 `sleep(10ms)` 实际会睡到 33ms，于是同样丢 46 个字，
//! 在快机器上算作 460ms、在慢机器上算作 1500ms——**测出来的是机器负载，
//! 不是磁盘的落后程度**。直接盯文件长度就没有这层换算。
//!
//! # 为什么给 500ms 抖动预算
//!
//! 1.5 秒是**设计承诺**，由 `ki-vault::autosave` 里 6 条注入时钟的单元测试
//! 严格证明（零抖动、零调度）。这里是端到端，worker 线程在满载的 CI runner
//! 上被推迟几百毫秒是常态。留 500ms 预算既能让 CI 稳定，又仍然是一个
//! 一数量级的判别器——debounce 实现会在这里差 10 倍以上。
//!
//! # 为什么这条能验出 debounce 与 throttle 的区别
//!
//! 换成 debounce 实现时，子进程**一直在打字**，计时器永远被重置，
//! 于是磁盘上的字符数会一直停在 0，磁盘落后量等于已耗时——这条会直接爆表。
//!
//! # 怎么跑
//!
//! ```sh
//! ./scripts/crash-note-save.sh              # 连同原子性一起跑
//! KI_TYPING_ROUNDS=50 cargo test -p ki-ipc --test crash_autosave -- --ignored --nocapture
//! ```

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ki_ipc::autosave;
use tempfile::TempDir;

const CHILD_VAULT: &str = "KI_TYPING_VAULT";

/// 打字节拍。10ms/字 = 100 字/秒，接近熟练打字员的持续速度。
const TICK_MS: u64 = 10;

/// 承诺的落盘间隔，与 `ki_vault::AUTOSAVE_INTERVAL` 同源。
const PROMISE_MS: f64 = 1500.0;

/// 调度抖动预算：worker 线程在满载 runner 上被推迟的余量。
///
/// 这是**测试**的宽容度，不是产品承诺。真正的 1.5 秒由注入时钟的单元测试
/// 严格锁定；这里只排除「设计坏了」这一类失败。
const JITTER_BUDGET_MS: f64 = 500.0;

/// 一个字的 UTF-8 字节数。子进程只往正文里加 `字`，所以长度 / 3 = 字符数。
const CHAR_BYTES: u64 = 3;

/* ───────────────────── 子进程：一直在打字 ───────────────────── */

/// 子进程主体。没有 `KI_TYPING_VAULT` 时立刻返回，
/// 所以它在普通 `cargo test` 里只是一条无副作用的空用例。
#[test]
fn child_typing() {
    let Ok(vault) = std::env::var(CHILD_VAULT) else {
        return;
    };

    let root = std::path::PathBuf::from(&vault);
    let note = root.join("note.md");
    let start = Instant::now();
    let mut content = String::new();
    let mut last_seen: u64 = 0;

    // 上限只是防止父进程意外消失后留下一个永动机。
    for typed in 1..=100_000u64 {
        content.push('字');
        // 走真实的自动保存路径：入队 + 节流 + 定时线程 + 原子写。
        autosave::submit(root.clone(), "note.md".to_owned(), content.clone(), None);

        // 盯磁盘：这是「用户手指」与「磁盘」之间真实的差距，不是推算。
        let on_disk = std::fs::metadata(&note).map(|m| m.len()).unwrap_or(0);
        if on_disk != last_seen {
            last_seen = on_disk;
            println!("FLUSHED {} {}", on_disk, start.elapsed().as_millis());
        }
        // 先报数再睡：父进程据此知道"此刻用户已经打出了多少字"。
        println!("TYPED {typed} {}", start.elapsed().as_millis());
        std::thread::sleep(Duration::from_millis(TICK_MS));
    }
}

/* ───────────────────── 父进程：随机硬杀并测量 ───────────────────── */

fn pseudo_random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

/// 从子进程 stdout 里读到的最新进度。
#[derive(Default, Clone, Copy)]
struct Progress {
    typed: u64,
    /// 最后一次 `TYPED` 行的 elapsed。
    elapsed_ms: u64,
    /// 最后一次观测到落盘时的 elapsed。`None` 表示还没见过落盘。
    flushed_ms: Option<u64>,
}

#[test]
#[ignore = "时序相关的外部进程测试，用 scripts/crash-note-save.sh 显式运行"]
fn autosave_loses_at_most_the_interval() {
    let rounds: u32 = std::env::var("KI_TYPING_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let exe = std::env::current_exe().expect("拿不到测试二进制路径");
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;

    let mut samples: Vec<f64> = Vec::with_capacity(rounds as usize);

    for round in 1..=rounds {
        let vault = TempDir::new().expect("建临时 Vault 失败");
        std::fs::write(vault.path().join("note.md"), "").expect("建空笔记失败");

        let mut child = Command::new(&exe)
            .args(["--exact", "child_typing", "--nocapture"])
            .env(CHILD_VAULT, vault.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动子进程失败");

        let stdout = child.stdout.take().expect("子进程没有 stdout");
        // 用共享变量而不是 channel：channel 一旦被 `recv` 消费，
        // 想"再看一眼最新值"就没了；而这里需要的正是"此刻最新"。
        let latest = Arc::new(Mutex::new(Progress::default()));
        let sink = Arc::clone(&latest);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next(), parts.next()) {
                    (Some("FLUSHED"), Some(_bytes), Some(ms)) => {
                        if let Ok(ms) = ms.parse::<u64>() {
                            sink.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .flushed_ms = Some(ms);
                        }
                    }
                    (Some("TYPED"), Some(typed), Some(ms)) => {
                        if let (Ok(typed), Ok(ms)) = (typed.parse::<u64>(), ms.parse::<u64>()) {
                            let mut guard = sink.lock().unwrap_or_else(PoisonError::into_inner);
                            guard.typed = typed;
                            guard.elapsed_ms = ms;
                        }
                    }
                    _ => {}
                }
            }
        });

        let snapshot = || *latest.lock().unwrap_or_else(PoisonError::into_inner);

        // 等子进程真的开始打字，否则这一轮什么也证明不了。
        let started = Instant::now();
        while snapshot().typed == 0 {
            if started.elapsed() > Duration::from_secs(30) {
                panic!("子进程 30 秒内一个字都没打出来");
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // **锚点**：等第一次真的落盘再开始计时。
        //
        // 不等的话，进程冷启动（spawn + dyld + 首次建临时目录）的偏斜会
        // 整体平移整个节拍：第一次落盘晚了 300ms，后面每一次都晚 300ms，
        // 于是「随机时刻硬杀」偶尔会量到一个被平移过的落后量。
        // macOS CI 上就是这样冒出一个 1816ms 的孤立样本。
        while snapshot().flushed_ms.is_none() {
            if started.elapsed() > Duration::from_secs(30) {
                panic!("子进程 30 秒内一次都没落盘，自动保存根本没工作");
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // 随机再等 0–1999ms：覆盖"刚落完盘"到"快要落盘"之间的整个周期。
        let extra = pseudo_random(&mut seed) % 2000;
        std::thread::sleep(Duration::from_millis(extra));

        child.kill().expect("kill 失败");
        child.wait().expect("wait 失败");

        // 给读取线程一点时间把管道里剩下的行读完。
        std::thread::sleep(Duration::from_millis(150));
        let progress = snapshot();

        let disk_text = std::fs::read_to_string(vault.path().join("note.md")).expect("笔记消失了");

        // 磁盘内容必须是"前 N 个字"的完整前缀，不允许中间缺字。
        // 这是原子写在自动保存路径上的复核——如果 rename 出问题，
        // 长度可能对得上但内容错位。
        assert!(
            disk_text.chars().all(|c| c == '字'),
            "第 {round} 轮：磁盘上出现了不属于输入的字符"
        );

        let Some(flushed_ms) = progress.flushed_ms else {
            println!("round {round:>2}: 没观测到落盘，本轮跳过");
            continue;
        };

        let on_disk = disk_text.len() as u64 / CHAR_BYTES;
        // 磁盘落后 = 被杀那一刻离最后一次落盘过了多久。
        let stale_ms = progress.elapsed_ms.saturating_sub(flushed_ms) as f64;
        samples.push(stale_ms);

        println!(
            "round {round:>2}: typed={:<5} on_disk={on_disk:<5} 磁盘落后 ≈ {stale_ms:>6.0}ms  (被杀于 {extra}ms)",
            progress.typed
        );
    }

    assert!(!samples.is_empty(), "一轮有效样本都没有，测试无法判定");

    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    let p50 = sorted[sorted.len() / 2];
    let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
    let max = sorted[sorted.len() - 1];

    println!("\n=== 断电丢失量：{} 轮随机硬杀 ===", samples.len());
    println!(
        "=== P50 {p50:.0}ms · P95 {p95:.0}ms · max {max:.0}ms（承诺 {PROMISE_MS:.0}ms + 抖动预算 {JITTER_BUDGET_MS:.0}ms）==="
    );

    let ceiling = PROMISE_MS + JITTER_BUDGET_MS;
    assert!(
        p95 <= ceiling,
        "P95 磁盘落后 {p95:.0}ms 超过 {ceiling:.0}ms —— 自动保存的间隔或定时线程有问题"
    );
    // 单点灾难上限：debounce 实现下磁盘会一直停在 0，落后量随耗时线性增长，
    // 20 轮里必然有一轮远超 2 倍承诺。这条抓的是「偶发但离谱」。
    assert!(
        max <= PROMISE_MS * 2.0,
        "最坏一轮磁盘落后 {max:.0}ms 超过 {:.0}ms —— 这不是调度抖动能解释的",
        PROMISE_MS * 2.0
    );
}
