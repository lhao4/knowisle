//! 回答 M1 的那个问题：**用户现在断电，丢多少？**
//!
//! `tests/crash.rs`（在 `ki-vault`）验的是**原子性**：崩溃后不许出现半截内容。
//! 本文件验的是**丢失量**：崩溃后最多丢多久的输入。两者是不同的问题——
//! 一个文件可以每字节都完整，但仍然落后于用户手指 10 分钟。
//!
//! # 怎么量
//!
//! 子进程以固定节拍（10ms）往同一个字符串尾部加一个字，每加一个就
//! `autosave::submit` 一次（走真实的节流器与定时线程），并打印
//! `TYPED <字符数> <已耗时ms>`。
//!
//! 父进程在随机时刻硬杀它，然后：
//!
//! ```text
//! 丢失字符数 = 子进程最后打印的字符数 − 磁盘上实际的字符数
//! 丢失毫秒   = 丢失字符数 × 实测平均节拍
//! ```
//!
//! 判据：**P95 ≤ 1500ms**。这就是「最多丢 1.5 秒」的可执行形式。
//!
//! # 为什么这条能验出 debounce 与 throttle 的区别
//!
//! 换成 debounce 实现时，子进程**一直在打字**，计时器永远被重置，
//! 于是磁盘上的字符数会一直停在 0，丢失量等于已耗时——这条测试会直接爆表。
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
///
/// 1.5 秒的承诺换算成字符就是 150 个——用字符数做判据比用墙钟更稳，
/// 因为 Windows 的 `sleep` 精度会让实际节拍偏离标称值。
const TICK_MS: u64 = 10;

/// 承诺的落盘间隔，与 `ki_vault::AUTOSAVE_INTERVAL` 同源。
const PROMISE_MS: f64 = 1500.0;

/* ───────────────────── 子进程：一直在打字 ───────────────────── */

/// 子进程主体。没有 `KI_TYPING_VAULT` 时立刻返回，
/// 所以它在普通 `cargo test` 里只是一条无副作用的空用例。
#[test]
fn child_typing() {
    let Ok(vault) = std::env::var(CHILD_VAULT) else {
        return;
    };

    let root = std::path::PathBuf::from(vault);
    let start = Instant::now();
    let mut content = String::new();

    // 上限只是防止父进程意外消失后留下一个永动机。
    for typed in 1..=100_000u64 {
        content.push('字');
        // 走真实的自动保存路径：入队 + 节流 + 定时线程 + 原子写。
        autosave::submit(root.clone(), "note.md".to_owned(), content.clone(), None);
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
        let latest = Arc::new(Mutex::new((0u64, 0u64)));
        let sink = Arc::clone(&latest);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("TYPED ") {
                    let mut parts = rest.split_whitespace();
                    if let (Some(typed), Some(ms)) = (parts.next(), parts.next()) {
                        if let (Ok(typed), Ok(ms)) = (typed.parse::<u64>(), ms.parse::<u64>()) {
                            *sink.lock().unwrap_or_else(PoisonError::into_inner) = (typed, ms);
                        }
                    }
                }
            }
        });

        // 等子进程真的开始打字，否则这一轮什么也证明不了。
        let typing_started = Instant::now();
        loop {
            let (typed, _) = *latest.lock().unwrap_or_else(PoisonError::into_inner);
            if typed > 0 {
                break;
            }
            if typing_started.elapsed() > Duration::from_secs(30) {
                panic!("子进程 30 秒内一个字都没打出来");
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
        let (typed, elapsed_ms) = *latest.lock().unwrap_or_else(PoisonError::into_inner);

        let disk_text = std::fs::read_to_string(vault.path().join("note.md")).expect("笔记消失了");
        let on_disk = disk_text.chars().count() as u64;

        // 磁盘内容必须是"前 N 个字"的完整前缀，不允许中间缺字。
        // 这是原子写在自动保存路径上的复核——如果 rename 出问题，
        // 长度可能对得上但内容错位。
        assert!(
            disk_text.chars().all(|c| c == '字'),
            "第 {round} 轮：磁盘上出现了不属于输入的字符"
        );

        if typed == 0 {
            // 一行都没读到（子进程可能启动极慢）：这一轮没有测量价值，跳过。
            println!("round {round:>2}: 子进程没来得及打字，本轮跳过");
            continue;
        }

        let lost_chars = typed.saturating_sub(on_disk);
        // 用实测平均节拍换算，而不是假设 10ms——Windows 的 sleep 精度会漂。
        let avg_tick = elapsed_ms as f64 / typed as f64;
        let lost_ms = lost_chars as f64 * avg_tick;
        samples.push(lost_ms);

        println!(
            "round {round:>2}: typed={typed:<5} on_disk={on_disk:<5} lost={lost_chars:<4} chars ≈ {lost_ms:>6.0}ms  (被杀于 {extra}ms)"
        );
    }

    assert!(!samples.is_empty(), "一轮有效样本都没有，测试无法判定");

    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    let p50 = sorted[sorted.len() / 2];
    let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
    let max = sorted[sorted.len() - 1];

    println!("\n=== 断电丢失量：{} 轮随机硬杀 ===", samples.len());
    println!("=== P50 {p50:.0}ms · P95 {p95:.0}ms · max {max:.0}ms（承诺 ≤ {PROMISE_MS:.0}ms）===");

    assert!(
        p95 <= PROMISE_MS,
        "P95 丢失 {p95:.0}ms 超过承诺的 {PROMISE_MS:.0}ms —— 自动保存的间隔或定时线程有问题"
    );
}
