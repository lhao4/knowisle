//! 待转存集合与批量转存的验收（Rust 侧）。
//!
//! 覆盖的是「数据从哪来、转完怎么自洽、失败怎么不连坐」这三件事。
//! 界面上那几项（不打开任何一篇就能数出 20 篇、进度、取消）在
//! `scripts/e2e/cdp-encoding.mjs` 里。

use std::fs;
use std::path::Path;

use ki_ipc::search::{encoding_stats, sync_after_scan};
use ki_ipc::transcode;
use ki_vault::encoding::{encode, Encoding};
use ki_vault::{scan_vault, ScanOptions};

fn vault() -> tempfile::TempDir {
    tempfile::tempdir().expect("创建临时目录")
}

fn sync(root: &Path) {
    let scan = scan_vault(root, &ScanOptions::default()).expect("扫描");
    sync_after_scan(root, &scan).expect("同步");
}

/// 写一篇 GBK，返回它的相对路径。
fn write_gbk(root: &Path, rel: &str, text: &str) {
    let abs = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).expect("建目录");
    }
    fs::write(&abs, encode(text, Encoding::Gbk).expect("编成 GBK")).expect("写 GBK");
}

/// 一篇 GBK 都认不出来时，这个功能就没有意义了。
fn assert_gbk(text: &str) -> String {
    format!("# 编码\n\n{text}\n")
}

#[test]
fn stats_list_only_the_notes_that_are_not_utf8() {
    let dir = vault();
    let root = dir.path();

    write_gbk(root, "gbk/a.md", &assert_gbk("第一篇 GBK 笔记"));
    write_gbk(root, "gbk/b.md", &assert_gbk("第二篇 GBK 笔记"));
    fs::write(root.join("utf8.md"), "# 本来就是 UTF-8\n").unwrap();
    // 带 BOM 的 UTF-8：`is_utf8()` 把 `Utf8Bom` 算作 UTF-8，所以它**不许**
    // 出现在待转存清单里。剥 BOM 是另一件事，不是编码转换。
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice("# 带 BOM 的 UTF-8\n".as_bytes());
    fs::write(root.join("bom.md"), &bom).unwrap();

    sync(root);
    let stats = encoding_stats(root).expect("统计");

    assert_eq!(stats.total, 2, "只该数出两篇 GBK");
    assert!(!stats.truncated);
    let listed: Vec<(&str, &str)> = stats
        .non_utf8
        .iter()
        .map(|entry| (entry.path.as_str(), entry.encoding.as_str()))
        .collect();
    // 按路径升序。`gbk/a.md` < `gbk/b.md`，而 `bom.md` / `utf8.md` 不在里面。
    assert_eq!(listed, vec![("gbk/a.md", "GBK"), ("gbk/b.md", "GBK")]);
}

/// 索引器不写这一列时，上面那条用例会红（`total` 是 0）。
///
/// 这条单独立着，是因为它是「数据从索引来」这个设计的**唯一证据**：
/// 没有它，`stats` 的实现可以偷偷改成扫盘，而那会花掉 9.9 秒。
#[test]
fn stats_come_from_the_index_not_from_the_disk() {
    let dir = vault();
    let root = dir.path();
    write_gbk(root, "a.md", &assert_gbk("GBK 笔记"));

    // 还没建索引：没有数据可给。这正是「不许在扫描里读正文」的代价，
    // 也是为什么返回值要带 `indexState`。
    let before = encoding_stats(root).expect("统计");
    assert_eq!(before.total, 0, "索引还没建，不许凭空数出 1 篇");

    sync(root);
    assert_eq!(encoding_stats(root).expect("统计").total, 1);
}

#[test]
fn transcoding_clears_the_column_without_touching_the_index_by_hand() {
    let dir = vault();
    let root = dir.path();
    write_gbk(root, "a.md", &assert_gbk("要转存的一篇"));
    write_gbk(root, "b.md", &assert_gbk("另一篇"));
    sync(root);
    assert_eq!(encoding_stats(root).expect("统计").total, 2);

    let batch = transcode::run_batch(root, vec!["a.md".to_string(), "b.md".to_string()], 1);
    assert_eq!(batch.ok.len(), 2);
    assert!(batch.failed.is_empty());
    assert!(!batch.cancelled);

    // 判据 4：没有手改索引行，`nonUtf8` 自己变成 0。
    // 机制是「转存改 mtime → 重索引 → `indexed_note` 重新算出 `None`」。
    let after = encoding_stats(root).expect("统计");
    assert_eq!(after.total, 0, "转存之后清单必须自己清空");
    assert!(after.non_utf8.is_empty());

    // 磁盘上真的是 UTF-8（判据 3 的 Rust 版）。
    for name in ["a.md", "b.md"] {
        let bytes = fs::read(root.join(name)).expect("读盘");
        let text = std::str::from_utf8(&bytes).expect("必须是合法 UTF-8");
        assert!(text.contains("要转存") || text.contains("另一篇"));
    }
}

/// 幂等：对已经是 UTF-8 的篇再跑一遍，0 篇需要转、`history/` 不新增。
#[test]
fn second_batch_is_a_no_op() {
    let dir = vault();
    let root = dir.path();
    write_gbk(root, "a.md", &assert_gbk("只此一篇"));
    sync(root);

    let first = transcode::run_batch(root, vec!["a.md".to_string()], 1);
    assert_eq!(first.ok.len(), 1);
    let history = root.join(ki_core::METADATA_DIR).join("history");
    let backups = count_files(&history);
    assert_eq!(backups, 1, "第一次必须留下且只留下一份备份");

    let second = transcode::run_batch(root, vec!["a.md".to_string()], 2);
    assert_eq!(second.ok.len(), 1, "幂等：已经是 UTF-8 也返回成功");
    assert_eq!(count_files(&history), backups, "幂等复跑不许新增备份");
}

/// 判据 6：失败不中断，且失败清单能定位到具体路径。
///
/// # 怎么造出「转存失败」
///
/// - Windows：独占句柄（`share_mode(0)`）占住文件，转存连读都读不到；
/// - Unix：父目录 `555`，tmp 建不出来（文件本身 444 在 rename 语义下
///   不一定失败——第八刀踩过这个坑）。
#[test]
fn failures_do_not_stop_the_batch_and_are_located_by_path() {
    let dir = vault();
    let root = dir.path();

    let names: Vec<String> = (0..20).map(|index| format!("d{index:02}.md")).collect();
    for name in &names {
        write_gbk(root, name, &assert_gbk(&format!("第 {name} 篇")));
    }

    let blocked = &names[3..6];
    let guard = block(blocked.iter().map(|name| root.join(name)).collect());

    let batch = transcode::run_batch(root, names.clone(), 7);

    assert_eq!(batch.ok.len(), 17, "其余 17 篇必须照转");
    assert_eq!(batch.failed.len(), 3, "失败的那 3 篇必须逐条记下来");
    let failed: Vec<&str> = batch.failed.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        failed,
        blocked.iter().map(String::as_str).collect::<Vec<_>>(),
        "失败清单必须能定位到路径，不能只报一个总数"
    );
    for failure in &batch.failed {
        // 原因必须是真的原因（不是"未知错误"）。
        let text = format!("{:?}", failure.error);
        assert!(
            text.contains("Io") || text.contains("PermissionDenied") || text.contains("Conflict"),
            "失败原因不可读：{text}"
        );
    }

    // 先放开占用，否则连断言自己都读不到那三篇（Windows 的 `share_mode(0)`
    // 不给任何共享权限，包括本进程的第二次 open）。
    drop(guard);

    // 成功的那些确实变了，失败的那些**一个字节都没动**。
    for name in &names {
        let bytes = fs::read(root.join(name)).expect("读盘");
        let is_utf8 = std::str::from_utf8(&bytes).is_ok();
        if blocked.contains(name) {
            assert!(!is_utf8, "{name} 转存失败了，磁盘不该被改");
        } else {
            assert!(is_utf8, "{name} 应该已经是 UTF-8");
        }
    }
}

fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += count_files(&path);
        } else {
            count += 1;
        }
    }
    count
}

/// 让这几篇**转存失败**，drop 时恢复。
struct Blocked {
    #[cfg(windows)]
    handles: Vec<fs::File>,
    #[cfg(unix)]
    dirs: Vec<std::path::PathBuf>,
}

#[cfg(windows)]
fn block(targets: Vec<std::path::PathBuf>) -> Blocked {
    use std::os::windows::fs::OpenOptionsExt;

    // `share_mode(0)`：不给任何人共享权限。转存的第一步 `File::open` 就会
    // 拿到共享冲突——那是一个 `Io` 错误，与「父目录只读」等价。
    let handles = targets
        .iter()
        .map(|path| {
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(path)
                .expect("占住文件")
        })
        .collect();
    Blocked { handles }
}

#[cfg(unix)]
fn block(targets: Vec<std::path::PathBuf>) -> Blocked {
    use std::os::unix::fs::PermissionsExt;

    let dirs: Vec<_> = targets
        .iter()
        .map(|path| {
            let dir = path.parent().expect("有父目录").to_path_buf();
            let mut perms = fs::metadata(&dir).expect("读目录权限").permissions();
            perms.set_mode(0o555);
            fs::set_permissions(&dir, perms).expect("设成只读");
            dir
        })
        .collect();
    Blocked { dirs }
}

impl Drop for Blocked {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.handles.clear();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in &self.dirs {
                let mut perms = fs::metadata(dir).expect("读目录权限").permissions();
                perms.set_mode(0o755);
                fs::set_permissions(dir, perms).expect("恢复可写");
            }
        }
    }
}
