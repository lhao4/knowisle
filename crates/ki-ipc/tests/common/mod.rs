//! 单字验收（`search_single_char.rs`）与单字基准（`single_char_latency.rs`）
//! 共用的夹具生成器。
//!
//! # 为什么是「真实语料」而不是模板
//!
//! `knowisle-index-bench` 的正文是同一个模板（每篇只差编号），于是它的单字
//! 命中面只有三档：0、3439（数字）、10000。而单字查询的成本结构恰好只由
//! **命中面**决定（候选 ≤1200 走精确路径、>1200 走截断路径），一份三档的
//! 夹具既量不出中间那一段，也挑不出「命中 ≤50 篇」的字。
//!
//! 这里从仓库自己的文档切 120 字的窗口，随机拼成 N 篇。语料是真的
//! （`docs/*.md` + `README.md`），字频分布也是真的，没有为了好看去调参——
//! 唯一的旋钮是篇数与每篇窗口数，它们只决定夹具体积。
//!
//! # 确定性
//!
//! 种子写死、窗口顺序由文档顺序决定，所以同一份文档生成同一份夹具。
//! 夹具跨轮复用（判据是 `.md` 篇数），复用不会把起点弄歪。

#![allow(dead_code)]

use std::fs;
use std::path::Path;

/// 一个语料窗口的字符数。
pub const WINDOW: usize = 120;
/// 每篇拼几个窗口。
pub const PER_NOTE: usize = 2;

/// 语料窗口：中文密度够高的行里，每 120 字切一段。
///
/// 密度闸是必须的：markdown 的表格与代码块切出来的窗口大半是 ASCII，
/// 混进来会把这个夹具变成「半份英文库」，单字命中面就不再是中文的样子。
pub fn corpus_windows() -> Vec<String> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sources = Vec::new();
    for name in [
        "ARCHITECTURE.md",
        "BACKLOG.md",
        "MVP-SCOPE.md",
        "M1-PERF.md",
        "M1-KICKOFF.md",
    ] {
        sources.push(fs::read_to_string(repo.join("docs").join(name)).expect("文档语料"));
    }
    sources.push(fs::read_to_string(repo.join("README.md")).expect("README"));

    let mut windows = Vec::new();
    for text in &sources {
        for line in text.lines() {
            let chars: Vec<char> = line.chars().collect();
            if chars.iter().filter(|c| ki_search::is_cjk(**c)).count() < 30 {
                continue;
            }
            for chunk in chars.chunks(WINDOW) {
                if chunk.iter().filter(|c| ki_search::is_cjk(**c)).count() < 30 {
                    continue;
                }
                windows.push(chunk.iter().collect::<String>());
            }
        }
    }
    assert!(
        windows.len() >= 100,
        "语料切出来的窗口太少（{}），夹具会退化成反复用同几段",
        windows.len()
    );
    windows
}

pub fn note_body(windows: &[String], rng: &mut XorShift, index: usize) -> String {
    let mut body = format!("# 随记 {index:05}\n\n");
    for _ in 0..PER_NOTE {
        body.push_str(&windows[rng.below(windows.len())]);
        body.push_str("\n\n");
    }
    body
}

pub fn build_fixture(root: &Path, files: usize, dirs: usize) {
    let windows = corpus_windows();
    let mut rng = XorShift::new(0x9E37_79B9_7F4A_7C15);
    if root.exists() {
        fs::remove_dir_all(root).expect("清掉上一轮的夹具");
    }
    for dir in 0..dirs {
        fs::create_dir_all(root.join(format!("d{dir:02}"))).unwrap();
    }
    for index in 0..files {
        let dir = format!("d{:02}", index % dirs);
        let path = root.join(dir).join(format!("n{index:05}.md"));
        fs::write(path, note_body(&windows, &mut rng, index)).unwrap();
    }
    println!("夹具已重建：{files} 篇 / {} 个语料窗口", windows.len());
}

/// 数 `.md`，跳过 `.knowisle/`（索引库不是笔记）。
pub fn count_notes(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".knowisle" {
            continue;
        }
        if entry.path().is_dir() {
            count += count_notes(&entry.path());
        } else if name.to_string_lossy().ends_with(".md") {
            count += 1;
        }
    }
    count
}

pub fn ensure_fixture(root: &Path, files: usize, dirs: usize) {
    if count_notes(root) != files {
        build_fixture(root, files, dirs);
    }
}

/// 把索引库整个删掉，让下一次 `sync_after_scan` 走「新建 + 全量读」。
///
/// # 为什么单字这两条用例必须自己重建，不能靠 `size + mtime` 跳过
///
/// 增量同步的判据是 `size + mtime`，而**切分规则**不在里面——它由
/// `TOKENIZER_VERSION` 兜底。正常改动会 bump 版本号，于是老库自动重建；
/// 但变异验证（改 `token_stream`、不改版本号）正好落在那个缺口里：上一轮
/// 写进去的是旧 token，这一轮同步看到文件没变就全部跳过，用例于是拿旧 token
/// 跑出**假绿**（或者像本刀那样，还原源码后跑出假红）。
///
/// 单字用例的成本全在「候选集对不对」上，一份脏索引会让它什么都没证明。
pub fn reset_index(root: &Path) {
    ki_ipc::search::invalidate(root);
    let meta = root.join(ki_core::METADATA_DIR);
    if meta.exists() {
        fs::remove_dir_all(&meta).expect("删掉上一轮的索引库");
    }
}

/// 把夹具读进内存：`(Vault 相对路径, 正文)`，按路径排序。
///
/// 相对路径用 `/` 分隔，与 `read_note` 返回的形态一致——`NoteLocator` 的
/// `name_hit` 判据依赖它，路径形态不同会让对拍在文件名上假红。
pub fn load_notes(root: &Path) -> Vec<(String, String)> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, String)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == ".knowisle" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.to_string_lossy().to_string()
            } else {
                format!("{prefix}/{}", name.to_string_lossy())
            };
            let path = entry.path();
            if path.is_dir() {
                walk(&path, &rel, out);
            } else if rel.ends_with(".md") {
                out.push((rel, fs::read_to_string(&path).unwrap_or_default()));
            }
        }
    }

    let mut out = Vec::new();
    walk(root, "", &mut out);
    out.sort();
    out
}

/// 自带的伪随机数发生器（xorshift64*）。只为测试可复现——`ki-search` 与
/// 这些测试都不引 `rand`。
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as usize
    }
}
