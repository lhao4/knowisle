//! 索引侧跳过清单的验收（Rust 侧）。
//!
//! # 它验的是「两批行互斥」与「数字与清单自洽」
//!
//! 界面上那几项（两个数分开显示、都能点开、0 条不出现入口）在
//! `scripts/e2e/cdp-skip-list.mjs` 里。这里验的是数据层：
//!
//! 1. 三种原因各自能定位到路径；
//! 2. 一篇要么在「待转存」、要么在「跳过」，**不许同时在两边**；
//! 3. 把那一篇删掉，清单自己少一条（没有手改索引）；
//! 4. 索引还没建时 `total` 是 0，且 `indexState` 在场——前端据此继续等。

use std::fs;
use std::path::Path;

use ki_core::IndexSkipReason;
use ki_ipc::search::{encoding_stats, skipped_notes, sync_after_scan};
use ki_vault::encoding::{encode, Encoding};
use ki_vault::{scan_vault, ScanOptions, MAX_INDEX_BYTES};

fn vault() -> tempfile::TempDir {
    tempfile::tempdir().expect("创建临时目录")
}

fn sync(root: &Path) {
    let scan = scan_vault(root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(root, &scan).expect("同步");
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let abs = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).expect("建目录");
    }
    fs::write(&abs, bytes).expect("写盘");
}

/// 一篇超过索引上限的笔记：**能读**，但索引器不读它。
fn huge_note() -> Vec<u8> {
    let mut bytes = "# 超大\n\n".as_bytes().to_vec();
    bytes.extend(std::iter::repeat_n(b'x', MAX_INDEX_BYTES as usize + 4096));
    bytes
}

/// 一篇**认不出编码**的笔记。
///
/// `0x01` 是判据：任何编码解出来都会带一个 `U+0001`，而 `is_plausible`
/// 把它当成「猜错了」。`0xFF` 保证严格 UTF-8 先失败（否则它根本走不到猜）。
///
/// # 长度必须是**奇数**
///
/// 实测踩过：`[0x01, 0xFF, 0x02, b'\n']` 重复 8 次（32 字节，偶数）会被
/// `sniff_utf16` 判成 UTF-16LE——`0x01 0xFF` 恰好是 `U+FF01`（`！`），
/// 解出来**没有控制字符**，于是它被当成一篇正常笔记索引了。奇数长度让
/// UTF-16 解码直接失败，才走得到「猜」那一步。
fn undecodable_note() -> Vec<u8> {
    let mut bytes = Vec::new();
    for _ in 0..7 {
        bytes.extend_from_slice(&[0x01, 0xFF, 0x02]);
    }
    bytes
}

#[test]
fn each_reason_is_located_by_path() {
    let dir = vault();
    let root = dir.path();

    write(root, "bad/huge.md", &huge_note());
    write(root, "bad/weird.md", &undecodable_note());
    write(root, "ok.md", "# 正常\n".as_bytes());
    // GBK 是**能解码**的：它在待转存清单里，不在跳过清单里。
    write(
        root,
        "bad/gbk.md",
        &encode("# 编码\n\n这篇是 GBK，解得出来。\n", Encoding::Gbk).expect("编成 GBK"),
    );

    sync(root);
    let stats = skipped_notes(root).expect("跳过清单");

    assert_eq!(stats.total, 2, "只有 2 篇没进索引");
    assert!(!stats.truncated, "2 条不该触发截断");
    let listed: Vec<(&str, IndexSkipReason)> = stats
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.reason))
        .collect();
    // 按路径升序：`bad/gbk.md` 不在里面（它进了索引），`bad/huge.md` < `bad/weird.md`。
    assert_eq!(
        listed,
        vec![
            ("bad/huge.md", IndexSkipReason::TooLarge),
            ("bad/weird.md", IndexSkipReason::InvalidUtf8),
        ],
        "每一种原因都要能定位到路径，不许只有一个总数"
    );
}

/// 验收 5：同一篇不许同时出现在「待转存」与「跳过」两个清单里。
///
/// 这条不是靠文案，是靠两条 WHERE 互斥：`src_encoding IS NOT NULL` 只可能
/// 出现在 `status='ok'` 的行上（`upsert_skipped` 会把那一列清成 NULL）。
#[test]
fn pending_and_skipped_lists_never_overlap() {
    let dir = vault();
    let root = dir.path();

    write(
        root,
        "gbk.md",
        &encode("# 待转存\n\n这篇编码不是 UTF-8，但读得出来。\n", Encoding::Gbk).unwrap(),
    );
    write(root, "weird.md", &undecodable_note());
    sync(root);

    let pending = encoding_stats(root).expect("待转存");
    let skipped = skipped_notes(root).expect("跳过");

    assert_eq!(pending.total, 1, "GBK 那篇在待转存里");
    assert_eq!(pending.non_utf8[0].path, "gbk.md");
    assert_eq!(skipped.total, 1, "认不出编码的那篇在跳过里");
    assert_eq!(skipped.entries[0].path, "weird.md");

    let pending_paths: Vec<&str> = pending.non_utf8.iter().map(|e| e.path.as_str()).collect();
    let skipped_paths: Vec<&str> = skipped.entries.iter().map(|e| e.path.as_str()).collect();
    for path in &skipped_paths {
        assert!(
            !pending_paths.contains(path),
            "{path} 同时出现在两个清单里：这正是「待转存 ≠ skipped」那条红线"
        );
    }
}

/// 验收 4：把超过上限的那篇删掉，索引侧少一条。
#[test]
fn deleting_the_huge_note_shrinks_the_list() {
    let dir = vault();
    let root = dir.path();

    write(root, "huge.md", &huge_note());
    write(root, "ok.md", "# 正常\n".as_bytes());
    sync(root);
    assert_eq!(skipped_notes(root).expect("跳过").total, 1);

    fs::remove_file(root.join("huge.md")).expect("删掉");
    sync(root);

    let after = skipped_notes(root).expect("跳过");
    assert_eq!(after.total, 0, "删掉之后清单必须自己变空");
    assert!(after.entries.is_empty());
}

/// 索引还没建时：`total` 是 0，但 `indexState` 必须在场。
///
/// 没有 `indexState`，前端会把「还没数到」读成「没有跳过」——入口永远不出现。
/// 这是第十一刀（ADR-012）踩过的坑，这里第二次不踩。
#[test]
fn empty_stats_still_carry_the_index_state() {
    let dir = vault();
    let root = dir.path();
    write(root, "huge.md", &huge_note());

    // 还没同步过：索引库是空的（甚至可能刚被建出来），一篇都没数到。
    let before = skipped_notes(root).expect("跳过");
    assert_eq!(before.total, 0, "索引没建，不许凭空数出 1 篇");
    assert!(before.entries.is_empty());

    sync(root);
    let after = skipped_notes(root).expect("跳过");
    assert_eq!(after.total, 1);
    // 建完之后状态不再是 `Building`：前端据此停止轮询。
    assert!(
        !matches!(after.index_state, ki_core::IndexState::Building { .. }),
        "同步跑完了，不该还在建：{:?}",
        after.index_state
    );
}
