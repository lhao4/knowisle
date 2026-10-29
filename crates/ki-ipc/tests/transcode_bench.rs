//! 单篇转存的代价：10k 真实语料上取 100 篇，量「读一遍 + 写一遍」的 p50。
//!
//! ```sh
//! cargo test -p ki-ipc --release --test transcode_bench -- --ignored --nocapture
//! ```
//!
//! # 这条基准在守什么
//!
//! 转存只该是「读一遍 + 写一遍」。出现整库操作（重新扫描、重建索引、
//! 逐篇 stat）就是路径写错了——而那种错误不会让任何功能用例变红，只会让
//! 用户点一下按钮之后转圈十几秒。
//!
//! 判据：单篇 p50 ≤ 20 ms。这个数字不是拍出来的：它要盖住两次 fsync
//! （目标 + 备份）与一次 rename，同时离「用户感知到卡顿」还很远。
//!
//! # 夹具为什么是「把语料改写成 GBK」而不是另造 100 篇
//!
//! 另造的 100 篇量的是另一份文件（大小、目录深度、杀软缓存都不同）。
//! 把 10k 语料里的 100 篇**原地**改成 GBK，量的就是真实库里的那一篇——
//! 包括它在 50 个父目录里的位置。跑完会把原文写回去，语料不留痕。
//!
//! 装不下 GBK 的篇（emoji、生僻字）跳过并计数：它们是另一类夹具，
//! 与「单篇转存要多久」无关。

mod common;

use std::fs;
use std::time::Instant;

use ki_vault::encoding::{encode, Encoding};
use ki_vault::{read_note, transcode_to_utf8};

const FILES: usize = 10_000;
const DIRS: usize = 50;
/// 取样篇数。100 个样本的 p50 足够稳，再多就只是把墙钟拉长。
const SAMPLES: usize = 100;
/// 取样步长。与 10000 互质，取到的是均匀铺开的 100 篇，不是前 100 篇。
const STRIDE: usize = 97;

#[test]
#[ignore = "10k 夹具 + 100 次真实写盘，显式跑"]
fn single_note_transcode_cost() {
    let root = std::env::temp_dir().join("knowisle-single-char");
    common::ensure_fixture(&root, FILES, DIRS);
    // 真实口径：用户对每篇只转存一次，`history/<rel>/` 那个目录**总是新建的**。
    // 不删的话第二轮开始 `create_dir_all` 变成空操作，量出来的数字比用户
    // 实际遇到的低 1ms 左右。
    let history = root.join(ki_core::METADATA_DIR).join("history");
    if history.exists() {
        fs::remove_dir_all(&history).expect("清掉上一轮的备份");
    }
    let notes = common::load_notes(&root);
    assert_eq!(notes.len(), FILES, "夹具篇数不对");

    // 取样并把它们改写成 GBK。`original` 留着跑完写回去。
    let mut samples: Vec<(String, String, i64)> = Vec::new();
    for (index, (rel, text)) in notes.iter().enumerate().step_by(STRIDE) {
        if samples.len() >= SAMPLES {
            break;
        }
        let Ok(bytes) = encode(text, Encoding::Gbk) else {
            continue;
        };
        let abs = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::write(&abs, &bytes).expect("写成 GBK");
        let note = read_note(&root, rel).expect("读回 GBK 那篇");
        assert_eq!(&note.content, text, "{rel} 的 GBK 往返丢了字");
        assert_eq!(note.decoded_from.as_deref(), Some("GBK"), "{rel} 不是 GBK");
        samples.push((rel.clone(), text.clone(), note.mtime));
        if index + STRIDE >= FILES && samples.len() < SAMPLES {
            break;
        }
    }
    assert!(
        samples.len() >= SAMPLES / 2,
        "样本太少（{}），语料里装不下 GBK 的篇太多",
        samples.len()
    );

    // 冷口径：先读一遍（不进计时），让文件进系统缓存——转存发生在用户
    // 刚打开的那一篇上，它一定在缓存里。
    for (rel, _, _) in &samples {
        let _ = read_note(&root, rel).expect("预热");
    }

    let mut read_only = Vec::with_capacity(samples.len());
    let mut total = Vec::with_capacity(samples.len());
    for (rel, text, mtime) in &samples {
        let started = Instant::now();
        let note = read_note(&root, rel).expect("读");
        read_only.push(started.elapsed());
        assert_eq!(&note.content, text);

        let started = Instant::now();
        let after = transcode_to_utf8(&root, rel, Some(*mtime)).expect("转存");
        total.push(started.elapsed());
        assert_eq!(&after.content, text, "{rel} 转存之后正文变了");
        assert_eq!(after.decoded_from, None);
    }

    // 原文写回去，语料不留痕（单字那几条用例共用这份夹具）。
    for (rel, text, _) in &samples {
        let abs = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::write(&abs, text).expect("写回 UTF-8");
    }

    let report = |name: &str, mut values: Vec<std::time::Duration>| {
        values.sort();
        let at = |p: f64| values[((values.len() as f64 - 1.0) * p).round() as usize];
        println!(
            "{name}: n={} p50={:.2}ms p95={:.2}ms max={:.2}ms",
            values.len(),
            at(0.50).as_secs_f64() * 1000.0,
            at(0.95).as_secs_f64() * 1000.0,
            at(1.0).as_secs_f64() * 1000.0,
        );
        at(0.50).as_secs_f64() * 1000.0
    };

    println!("夹具：{FILES} 篇 / {DIRS} 个父目录；样本 {} 篇", samples.len());
    let read_p50 = report("read+decode（对照）", read_only);
    let total_p50 = report("转存（读+备份+原子写+重读）", total);
    println!(
        "差额（备份 + 原子写 + 重读）：{:.2}ms",
        total_p50 - read_p50
    );
    assert!(
        total_p50 <= 20.0,
        "单篇转存 p50 {total_p50:.2}ms 超过 20ms：转存只该是读一遍 + 写一遍"
    );
}
