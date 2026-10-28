//! 10k 真实语料上的**误判率**：必须是 0。
//!
//! ```sh
//! cargo test -p ki-ipc --release --test encoding_corpus -- --ignored --nocapture
//! ```
//!
//! # 这条判据在防什么
//!
//! 编码探测是「猜」，而猜错的代价是拿用户的数据换一个功能。所以本刀有一条
//! 硬判据：**任何一篇本来就是 UTF-8 的笔记，读出来的正文必须与改前逐字节
//! 相同**。不是抽样，是逐篇。
//!
//! # 为什么它几乎不可能红（但仍然要写）
//!
//! 探测顺序里「严格 UTF-8」排在「猜」之前，所以结构上就没有猜的机会。
//! 但结构上的保证会被后来的重构悄悄改掉（把 NUL 模式挪到 `from_utf8`
//! 之后、或者给 `detect` 加一个「先猜再看」的捷径），而那种改动不会让
//! 任何现有用例变红——它只会让某些用户的笔记变成乱码。
//!
//! 这条用例把「改前」钉成一个独立的判据：`fs::read_to_string` 是标准库的
//! 严格 UTF-8 读取，与我们的实现没有任何共享代码。

mod common;

use std::path::{Path, PathBuf};

use ki_vault::{read_note, scan_vault, ScanOptions};

const FILES: usize = 10_000;
const DIRS: usize = 50;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-single-char")
}

#[test]
#[ignore = "10k 夹具 + 逐篇读盘，显式跑"]
fn every_utf8_note_reads_byte_for_byte_as_before() {
    let root = fixture_root();
    common::ensure_fixture(&root, FILES, DIRS);

    let scan = scan_vault(&root, &ScanOptions::default()).expect("扫描");
    let files = ki_vault::flatten_files(&scan.tree);
    assert_eq!(files.len(), FILES, "夹具篇数不对");

    let mut misdetected = Vec::new();
    for file in &files {
        // 改前的行为就是这个：标准库的严格 UTF-8 读取。
        let before = std::fs::read_to_string(
            root.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR)),
        )
        .expect("夹具本身就是 UTF-8");
        let after = read_note(Path::new(&root), &file.path).expect("必须能读");
        if after.content != before || after.decoded_from.is_some() {
            misdetected.push((file.path.clone(), after.decoded_from.clone()));
        }
    }

    println!(
        "{} 篇全部比对完毕，误判 {} 篇",
        files.len(),
        misdetected.len()
    );
    assert!(
        misdetected.is_empty(),
        "误判 = 把好的 UTF-8 猜成别的编码，这是拿用户数据换功能：{:?}",
        &misdetected[..misdetected.len().min(10)]
    );
}
