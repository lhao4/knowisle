//! 门禁①：**误伤的规模**，以及「想知道被吞了多少」的代价（10k 夹具）。
//!
//! ```sh
//! cargo test -p ki-vault --release --test ignore_gate -- --ignored --nocapture
//! ```
//!
//! # 它回答三个数
//!
//! | 记号 | 问题 |
//! |---|---|
//! | ① | 内置名单在一份「真实感」夹具上剪掉了几个目录 |
//! | ② | 其中几个**含 `.md`**（要额外遍历才知道） |
//! | ③ | 为了知道 ② 要多走多少时间，占扫描的百分之几 |
//!
//! # 为什么这三个数决定了 §5 最后一条红线
//!
//! 剪枝意味着**没有下降**（`filter_entry`），所以「被剪掉的子树里有几篇笔记」
//! 这个数扫描器手上根本没有。要报它，就得把 `.git` / `node_modules` 再走一遍
//! ——那正是当初剪掉它们的理由。③ 量出来就是这个价格。
//!
//! 结论写进 ADR-014：**`ScanResult` 不报「被忽略了多少篇」**。用户要的是
//! 「为什么我的目录不见了」，答案在规则面板里（「`dist` 在内置名单里，你可以
//! negate 它」），不在一个会让他以为「只有 3 篇被吞」的计数里。
//!
//! # 夹具为什么叫「真实感」
//!
//! 纯 10k 篇平铺的库一个目录都不会被剪，量出来是 0 —— 那个数是真的，但没用。
//! 真正会咬人的形状是：用户把自己的库放在一个前端仓库旁边，或者从 Logseq
//! 迁过来。所以夹具里必须有 `logseq/pages/`、`dist/`、`node_modules/`，
//! 以及最容易被忽略的那一种——**嵌在用户目录里的** `my-notes/dist/`。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ki_core::ignore::BUILTIN_DIRS;
use ki_vault::{scan_vault, ScanOptions};

const FILES: usize = 10_000;
const DIRS: usize = 50;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join("knowisle-ignore-gate")
}

fn note(index: usize) -> String {
    format!("# 笔记 {index:05}\n\n这一篇用来测量扫描的固定开销：路径解析与一次 stat。\n")
}

/// 造一棵「用户的库 + 旁边一堆别人家的目录」的树。
///
/// 返回**故意造出来的**被剪子树里的 `.md` 篇数，用来核对量出来的数与夹具一致。
fn build_fixture(root: &Path) -> usize {
    if root.exists() {
        fs::remove_dir_all(root).expect("清掉上一轮的夹具");
    }
    for dir in 0..DIRS {
        fs::create_dir_all(root.join(format!("d{dir:02}"))).unwrap();
    }
    for index in 0..FILES {
        let dir = format!("d{:02}", index % DIRS);
        fs::write(
            root.join(dir).join(format!("n{index:05}.md")),
            note(index),
        )
        .unwrap();
    }

    // 用户自己的目录：里面有笔记，也有一个叫 dist 的子目录（误伤的正中央）。
    fs::create_dir_all(root.join("my-notes").join("dist")).unwrap();
    fs::write(root.join("my-notes").join("keep.md"), "# 保留\n").unwrap();

    let mut swallowed = 0usize;
    let mut seed = |rel: &str, count: usize, body: &str| {
        fs::create_dir_all(root.join(rel)).unwrap();
        for index in 0..count {
            fs::write(root.join(rel).join(format!("{index:03}.md")), body).unwrap();
        }
        swallowed += count;
    };

    // Logseq 图谱：`pages/` 是用户的笔记，`bak/` 是全量历史副本。
    seed("logseq/pages", 200, "# logseq page\n");
    seed("logseq/bak", 50, "# 旧副本\n");
    // 前端仓库的产物与依赖。
    seed("dist", 40, "# 构建产物里的 md\n");
    seed("node_modules/react", 30, "# 别人的 README\n");
    seed("node_modules/vue", 20, "# 别人的 README\n");
    // 嵌在用户目录里的那一份——按名字匹配，所以它也被吞。
    seed("my-notes/dist", 20, "# 用户的 dist\n");
    // Obsidian 的插件目录。
    seed(".obsidian/plugins/foo", 5, "# 插件文档\n");

    // 纯附件目录：不该影响任何计数。
    fs::create_dir_all(root.join("assets")).unwrap();
    fs::write(root.join("assets").join("pic.png"), "png").unwrap();

    swallowed
}

/// 走一棵子树，数「子目录数 / `.md` 篇数」，并回答「这棵子树里有没有 `.md`」。
fn walk(dir: &Path, dirs: &mut usize, notes: &mut usize) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut has_md = false;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            *dirs += 1;
            has_md |= walk(&entry.path(), dirs, notes);
        } else if entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            has_md = true;
            *notes += 1;
        }
    }
    has_md
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn scan_once(root: &Path) -> (Duration, usize) {
    let started = Instant::now();
    let result = scan_vault(root, &ScanOptions::default()).expect("扫描");
    (started.elapsed(), result.total_files)
}

#[test]
#[ignore = "门禁：需要 10k 夹具与 release，用 --ignored 显式跑"]
fn measure_the_cost_of_knowing_what_was_swallowed() {
    let root = fixture_root();
    let planted = build_fixture(&root);

    // 预热一次：第一次含页缓存冷启动，不预热会把「冷」算成「配置的成本」。
    let (_, total) = scan_once(&root);
    assert_eq!(total, FILES + 1, "10k + my-notes/keep.md");

    // ① 被剪掉的目录：只数**命中内置名单的那一层**，子树不算（我们没下去）。
    let mut pruned = 0usize;
    for entry in fs::read_dir(&root).unwrap().flatten() {
        if entry.file_type().map(|k| k.is_dir()).unwrap_or(false) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if BUILTIN_DIRS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&name))
            {
                pruned += 1;
            }
        }
    }
    // 嵌在用户目录里的那一份也要算：`my-notes/dist`。
    if root.join("my-notes").join("dist").is_dir() {
        pruned += 1;
    }

    // ②③ 想知道「被吞了多少」：把每一棵被剪的子树再走一遍。
    //
    // 被剪的位置写死在这里是**故意的**：它模拟的正是「用户问『我少了什么』，
    // 我们只能自己再走一遍」这条路径——扫描器本身不知道这些目录在哪，
    // 因为它根本没访问过它们。
    const SWALLOWED: &[&str] = &["logseq", "dist", "node_modules", ".obsidian", "my-notes/dist"];

    let mut extra = Vec::new();
    let mut dirs = 0usize;
    let mut with_md = 0usize;
    let mut notes = 0usize;
    for _ in 0..3 {
        dirs = 0;
        with_md = 0;
        notes = 0;
        let started = Instant::now();
        for rel in SWALLOWED {
            let path = root.join(rel);
            if !path.is_dir() {
                continue;
            }
            if walk(&path, &mut dirs, &mut notes) {
                with_md += 1;
            }
        }
        extra.push(started.elapsed());
    }
    assert_eq!(notes, planted, "量出来的篇数必须等于夹具造出来的");

    let scans: Vec<Duration> = (0..3).map(|_| scan_once(&root).0).collect();
    let scan = median(scans);
    let extra = median(extra);
    let ratio = extra.as_secs_f64() / scan.as_secs_f64() * 100.0;

    println!("── 门禁① 误伤的规模（10k 夹具，release）──");
    println!("被内置名单剪掉的目录：{pruned} 个");
    println!("其中含 .md 的目录：  {with_md} 个（子树内共 {dirs} 个目录）");
    println!("被吞掉的 .md：        {notes} 篇");
    println!("扫描（中位）：        {scan:.2?}");
    println!("只为知道这些数：      {extra:.2?}（+{ratio:.1}%）");
    println!("── 结论：这个数不进 ScanResult（ADR-014）──");

    assert!(pruned >= 5, "夹具必须真的踩中内置名单");
    assert!(notes > 0, "夹具里必须有被吞掉的笔记");
}

/* ───────────────────────── 门禁②：读配置的成本 ───────────────────────── */

/// 门禁②：`<vault>/.knowisle/config.toml`（< 1 KB）读 + 解析对 `vault_scan` 的增量。
///
/// 门禁 **≤ 2%**。超了说明在扫描热路径上干了不该干的事——最典型的是「每个
/// 目录 stat 一次配置」。这里量的是「开头读一次」的价格。
///
/// # 为什么 A/B 都在同一个二进制里
///
/// 跨构建比（改前 vs 改后各编一次）会把编译差异、页缓存、Defender 的随机
/// 抖动一起算进去。同一进程里只切 `IgnoreSource` 这一个变量，差出来的
/// 就真的是那一次 `read_to_string` + 解析。
#[test]
#[ignore = "门禁：需要 10k 夹具与 release，用 --ignored 显式跑"]
fn measure_the_cost_of_reading_the_config_once() {
    let root = fixture_root();
    if !root.exists() {
        build_fixture(&root);
    }

    // 一份「真实大小」的配置：两个数组、几条规则、带注释。
    //
    // **规则刻意不改变这棵树**（`extra` 里都是夹具里没有的名字）。否则量出来的
    // 是「多扫 290 篇笔记」的成本，不是「读一次配置」的成本——那会把一个
    // 0.1 ms 的读盘放大成 2 ms 的遍历，然后我们就会去优化一个不存在的问题。
    let config = "\
# 我的忽略规则（知屿）
[ignore]
extra = [
  \"我的笔记\",      # 草稿目录
  \"drafts\",
  \"私人\",
  \"附件\",
  \"导出\",
  \"归档\",
]
negate = []
";
    assert!(config.len() < 1024, "夹具配置必须 < 1 KB");
    let path = ki_vault::config_path(&root);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, config).unwrap();

    let base_opts = ScanOptions {
        ignore: ki_vault::IgnoreSource::BuiltinOnly,
        ..ScanOptions::default()
    };
    let with_opts = ScanOptions::default();
    assert_eq!(with_opts.ignore, ki_vault::IgnoreSource::VaultConfig);

    // 各预热一次：第一次含页缓存冷启动。
    let base_total = scan_vault(&root, &base_opts).unwrap().total_files;
    let with_total = scan_vault(&root, &with_opts).unwrap().total_files;
    assert_eq!(base_total, with_total, "前提：这份配置不改变树");

    let mut base = Vec::new();
    let mut with = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let _ = scan_vault(&root, &base_opts).unwrap();
        base.push(started.elapsed());

        let started = Instant::now();
        let result = scan_vault(&root, &with_opts).unwrap();
        with.push(started.elapsed());

        // 规则确实被读进来了（否则量的是「读了个寂寞」）。
        assert_eq!(result.ignore_rules.extra.len(), 6);
        assert_eq!(result.total_files, base_total);
    }

    let base = median(base);
    let with = median(with);
    let delta = (with.as_secs_f64() - base.as_secs_f64()) / base.as_secs_f64() * 100.0;

    // 绝对成本：单独把 `load` 跑 1000 次。40 ms 的扫描里那一次读盘究竟值多少，
    // 靠整次扫描的差值看不出来（噪声比它大），只能单独量。
    let rounds = 1000;
    let started = Instant::now();
    for _ in 0..rounds {
        let rules = ki_vault::load_ignore_rules(&root);
        assert!(rules.config_error.is_none());
    }
    let per_call = started.elapsed() / rounds;

    // 拆开看：同一个库，配置不存在时只剩「一次失败的 open」。
    // 两者的差才是读 + 解析的价格；没有这个差，402µs 会被误读成「解析很慢」。
    let empty = root.join("no-config-here");
    fs::create_dir_all(&empty).unwrap();
    let started = Instant::now();
    for _ in 0..rounds {
        let rules = ki_vault::load_ignore_rules(&empty);
        assert!(rules.config_error.is_none() && rules.extra.is_empty());
    }
    let missing = started.elapsed() / rounds;

    println!("── 门禁② 读一次配置的成本（10k 夹具，release）──");
    println!("不读配置（中位）：{base:.2?}");
    println!("读配置（中位）：  {with:.2?}");
    println!("整次扫描的增量：  {delta:+.2}%（门禁 ≤ 2%）");
    println!("单次 load：       {per_call:.2?}（占 {:.3}%）", per_call.as_secs_f64() / base.as_secs_f64() * 100.0);
    println!("其中「文件不存在」：{missing:.2?}（一次失败的 open）");
    println!("读 + 解析的净成本：{:.2?}", per_call.saturating_sub(missing));
    println!("配置体积：        {} B", config.len());

    assert!(
        delta <= 2.0,
        "读配置的增量 {delta:+.2}% 超过 2% 门禁：扫描热路径上干了不该干的事"
    );
}
