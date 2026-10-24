//! 崩溃一致性：写入过程中被硬杀，磁盘上只允许出现「完整的旧内容」或「完整的新内容」。
//!
//! # 怎么杀
//!
//! 子进程不停地把 1MB 正文写进同一篇笔记，父进程在**随机时刻**硬杀它。
//! `std::process::Child::kill()` 在 Unix 上就是 `SIGKILL`（`kill -9`），
//! 在 Windows 上是 `TerminateProcess` —— 两者都是**不给任何清理机会**的硬杀，
//! 所以这条用例在两个平台上验的是同一件事。
//!
//! # 为什么这能证明原子写成立
//!
//! 如果实现是 `fs::write`（`O_TRUNC` + 多次 `write(2)`），硬杀会留下两种残骸：
//!
//! - **0 字节**：截断已经发生、数据还没写；
//! - **半截**：前 N 字节是新一代内容，后面是上一代。
//!
//! 两种都能被本用例抓住：校验方式是「整份 1MB 必须逐字节等于某一代的完整内容」，
//! 长度不对、或尾部对不上，都算失败。
//!
//! # 怎么跑
//!
//! 默认被 `#[ignore]` 跳过（它是时序相关的外部进程测试，不该混在日常 `cargo test` 里）：
//!
//! ```sh
//! ./scripts/crash-note-save.sh          # 20 轮
//! KI_CRASH_ROUNDS=100 cargo test -p ki-vault --test crash -- --ignored --nocapture
//! ```

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ki_vault::{read_note, write_note};
use tempfile::TempDir;

/// 正文尺寸。1MB 足够大到「一次 write 写不完」，又不至于让 20 轮跑成分钟级。
const SIZE: usize = 1024 * 1024;

const CHILD_VAULT: &str = "KI_CRASH_CHILD_VAULT";
const CHILD_REL: &str = "KI_CRASH_CHILD_REL";

/// 第 `gen` 代正文：整份 1MB 由 8 位十六进制代号循环填充。
///
/// 头 8 字节就是代号本身，所以校验时能先解出「这是第几代」，再逐字节比对整份内容。
/// 这样任何「头对了、尾巴是旧的」的缝合怪都会被抓出来。
fn content_of(gen: u64) -> String {
    let seed = format!("{gen:08x}");
    seed.chars().cycle().take(SIZE).collect::<String>()
}

fn parse_generation(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != SIZE {
        return None;
    }
    let head = std::str::from_utf8(&bytes[..8]).ok()?;
    u64::from_str_radix(head, 16).ok()
}

/// 极简伪随机：只要够「不可预测地落在写入的不同阶段」，不需要统计学质量。
fn pseudo_random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

fn tmp_leftovers(vault: &Path) -> usize {
    std::fs::read_dir(vault)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(".knowisle.tmp.")
                })
                .count()
        })
        .unwrap_or(0)
}

/* ───────────────────── 子进程：不停写 ───────────────────── */

/// 子进程主体。
///
/// 不带 `KI_CRASH_CHILD_VAULT` 时立刻返回 —— 所以它在普通 `cargo test` 里
/// 只是一条无副作用的空用例。
#[test]
fn child_writer() {
    let (Ok(vault), Ok(rel)) = (std::env::var(CHILD_VAULT), std::env::var(CHILD_REL)) else {
        return;
    };

    let root = PathBuf::from(vault);
    // 上限只是防止父进程意外消失后留下一个永动机。
    for gen in 1..=100_000u64 {
        write_note(&root, &rel, &content_of(gen)).expect("子进程写入失败");
        // 每次提交后报一声，父进程据此确认「至少写成过一次」再动手杀。
        println!("COMMITTED {gen}");
    }
}

/* ───────────────────── 父进程：随机时刻硬杀 ───────────────────── */

#[test]
#[ignore = "时序相关的外部进程测试，用 scripts/crash-note-save.sh 显式运行"]
fn crash_never_leaves_a_partial_note() {
    let rounds: u32 = std::env::var("KI_CRASH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let exe = std::env::current_exe().expect("拿不到测试二进制路径");
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;

    let mut killed_during_write = 0u32;
    let mut leftover_total = 0usize;

    for round in 1..=rounds {
        let vault = TempDir::new().expect("建临时 Vault 失败");
        let rel = "note.md";

        // 起点：磁盘上是第 0 代（旧内容）。
        write_note(vault.path(), rel, &content_of(0)).expect("写入初始内容失败");

        let mut child = Command::new(&exe)
            .args(["--exact", "child_writer", "--nocapture"])
            .env(CHILD_VAULT, vault.path())
            .env(CHILD_REL, rel)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动子进程失败");

        // 等子进程报出第一次提交 —— 否则「杀掉后文件没变」可能只是因为
        // 它还没开始写，那样的轮次什么也证明不了。
        let stdout = child.stdout.take().expect("子进程没有 stdout");
        let (tx, rx) = mpsc::channel::<u64>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("COMMITTED ") {
                    if let Ok(gen) = rest.trim().parse::<u64>() {
                        if tx.send(gen).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let first = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("子进程 30 秒内一次都没写成，无法判定");
        assert!(first >= 1, "第一次提交的代次异常：{first}");

        // 随机再等 0–149ms。1MB 的「写 + 两次 fsync」在普通盘上约 20–40ms，
        // 所以这个区间能覆盖到写入的各个阶段：写数据中、fsync 中、rename 前后。
        // 区间太窄（比如只等 0–39ms）会让每一轮都停在第二次写入里，方差不够。
        let extra = pseudo_random(&mut seed) % 150;
        std::thread::sleep(Duration::from_millis(extra));

        child.kill().expect("kill 失败");
        child.wait().expect("wait 失败");

        // ── 重启后校验 ──
        let bytes = std::fs::read(vault.path().join(rel)).expect("目标文件消失了");
        let gen = parse_generation(&bytes).unwrap_or_else(|| {
            panic!(
                "第 {round} 轮：文件长度 {} 字节，不是任何一代的完整内容（期望 {SIZE}）",
                bytes.len()
            )
        });
        let expected = content_of(gen);
        assert!(
            bytes == expected.as_bytes(),
            "第 {round} 轮：代号 {gen} 解析成功但内容对不上——出现半截写入"
        );

        // 内容还必须真的被换过：停在 0 代说明杀早了，这一轮没有验证价值。
        assert!(
            gen >= 1,
            "第 {round} 轮：杀之前没有完成任何一次提交，本轮无效"
        );
        killed_during_write += 1;

        // 顺便用公开 API 复核一遍：读得回来，且读到的就是那一代。
        let read_back = read_note(vault.path(), rel).expect("重启后读取失败");
        assert_eq!(read_back.size, SIZE as u64);
        assert_eq!(read_back.content.len(), SIZE);
        assert!(
            read_back.content.starts_with(&format!("{gen:08x}")),
            "第 {round} 轮：read_note 读到的不是同一代"
        );

        let leftovers = tmp_leftovers(vault.path());
        leftover_total += leftovers;
        println!("round {round:>2}: gen={gen:<6} delay={extra:>2}ms leftovers={leftovers}");
    }

    println!(
        "\n=== 崩溃一致性：{killed_during_write}/{rounds} 轮在写入中途硬杀，0 轮出现半截内容 ==="
    );
    println!(
        "=== 每轮平均遗留孤儿 tmp：{:.1} 个（不参与校验，见 BACKLOG）===",
        leftover_total as f64 / rounds as f64
    );
}
