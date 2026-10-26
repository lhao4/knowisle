//! 字面定位器与进程内全文索引。
//!
//! # 这不是搜索引擎
//!
//! 匹配规则只有一条：**ASCII 大小写折叠之后的子串匹配**。不模糊、不分词、
//! 不支持正则。中文天然可用——UTF-8 的子串匹配对中文是精确的。
//!
//! 为什么不做模糊：用户按 `Cmd+Shift+F` 时想找的是"哪篇笔记写了这句话"，
//! 不是"哪篇笔记的字母顺序最接近"。模糊匹配在文件名上是对的（见 `quick-open`），
//! 在正文上只会把噪音排到前面。
//!
//! # 两个角色，一个实现
//!
//! - [`NoteLocator`]：**给定一篇正文**，找出所有命中行（行号、UTF-16 列号、摘要窗口）。
//!   它是精度的唯一来源，且与索引无关——`ki-search` 的 FTS5 只决定"哪些笔记进候选"，
//!   进池之后仍然由它逐行字面判定。
//! - [`SearchIndex`]：M2 的进程内实现，把整个 Vault 读进内存再逐篇调定位器。
//!   M4 之后它是**旧引擎**，与新引擎共用同一个 `NoteLocator`——这是"新旧对拍"
//!   能成立的前提：两条路径的判定逻辑是同一份代码，差异只可能来自候选集。
//!
//! # 偏移量约定
//!
//! `fold_ascii` 只映射 `A-Z`，**不改变字节长度**。因此小写副本里的字节偏移
//! 可以直接拿去切原文，不需要任何偏移换算。这是"只折叠 ASCII"这个决定
//! 带来的第二个好处（第一个是中文天然可用）。

use std::path::{Path, PathBuf};

use ki_core::{IndexState, KiError, SearchHit, SearchResult};

use crate::scan::{read_note, scan_vault, ScanOptions};

/// 单篇笔记进索引的上限。
///
/// 比 `read_note` 的 10MB 更严：索引条目会在整个会话里驻留内存，而且每次查询
/// 都要扫一遍。2MB 的 Markdown 已经是病态（约 100 万字），它仍然可以正常打开
/// 编辑，只是搜不到——这个取舍必须让用户看得见（结果里报 `skippedFiles`）。
pub const MAX_INDEX_BYTES: u64 = 2 * 1024 * 1024;

/// 每篇最多返回几条命中行。
///
/// 一篇笔记里同一个词出现 50 次，对"找到这篇"没有额外帮助，只会把别的笔记
/// 挤出前 200 条。
pub const MAX_HITS_PER_FILE: usize = 3;

/// 一次搜索最多返回多少条命中行。
pub const MAX_HITS: usize = 200;

/// 命中片段往前保留多少个字符的上下文。
const SNIPPET_BEFORE: usize = 60;

/// 命中片段的总长度上限（字符数）。
///
/// 结果列表一行最多显示两行文本；240 字符已经超过两行的容量。
/// 它的主要作用不是省流量，而是让"一行 5000 字"的笔记不会把列表撑变形。
const SNIPPET_CHARS: usize = 240;

/// 一篇正文的定位器。
///
/// 构造之后只读。它同时持有原文与小写副本（内存约为正文两倍），
/// 换来的是"一次折叠、多次查询"。
pub struct NoteLocator {
    /// Vault 相对路径（POSIX）。同时是排序键与二分查找键。
    path: String,
    /// 文件名的 ASCII 小写形式。只用来判断「文件名是否也命中」。
    name_lower: String,
    /// 原文。
    content: String,
    /// 原文的 ASCII 小写副本。**字节长度与 `content` 完全相同。**
    lower: String,
    /// 每一行的起始字节偏移。`line_starts[0]` 恒为 0。
    line_starts: Vec<u32>,
}

impl NoteLocator {
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        let path = path.into();
        let content = content.into();
        let name_lower = name_lower_for_path(&path);
        let lower = fold_ascii(&content);
        let line_starts = line_starts(&content);
        Self {
            path,
            name_lower,
            content,
            lower,
            line_starts,
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// 文件名的 ASCII 小写形式（已剥扩展名）。
    pub fn name_lower(&self) -> &str {
        &self.name_lower
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    /// 找出这篇正文里最多 `max` 条命中行。
    ///
    /// 传入的是**原始 query**（内部自行 trim + 折叠）。查询为空时返回空——
    /// 空 needle 会让 `find` 在偏移 0 处恒命中，把每篇都变成一条命中。
    pub fn hits(&self, query: &str, max: usize) -> Vec<SearchHit> {
        let needle = fold_ascii(query.trim());
        if needle.is_empty() {
            return Vec::new();
        }
        self.hits_with_needle(&needle, max)
    }

    /// 已经折叠好的 needle。批量查询时省掉每篇一次 `fold_ascii` 的分配。
    pub(crate) fn hits_with_needle(&self, needle: &str, max: usize) -> Vec<SearchHit> {
        if needle.is_empty() || max == 0 {
            return Vec::new();
        }
        let name_hit = self.name_lower.contains(needle);
        let mut hits: Vec<SearchHit> = Vec::with_capacity(max.min(MAX_HITS_PER_FILE));
        let mut cursor = 0usize;

        // 每轮最多产出一条命中，然后跳到下一行——同一行的后续命中不再单独成条。
        // 注意 `hits.len()` 而不是 `cursor`：跨行被跳过的命中不消耗每篇的配额。
        while hits.len() < max {
            let Some(offset) = self.lower[cursor..].find(needle) else {
                break;
            };
            let at = cursor + offset;
            if let Some(hit) = self.make_hit(at, needle.len(), name_hit) {
                hits.push(hit);
            }
            cursor = self.next_line_start(at);
            if cursor >= self.content.len() {
                break;
            }
        }

        hits
    }

    /// 命中位置所在行的下一行起始偏移。用来保证"一行只出一条命中"。
    fn next_line_start(&self, at: usize) -> usize {
        let index = line_index(&self.line_starts, at);
        match self.line_starts.get(index + 1) {
            Some(&next) => next as usize,
            None => self.content.len(),
        }
    }

    /// 组装一条命中；命中横跨两行时返回 `None`。
    ///
    /// `at` 是命中在**小写副本**里的字节偏移；因为折叠不改变字节长度，
    /// 它同时是原文里的偏移。
    fn make_hit(&self, at: usize, len: usize, name_hit: bool) -> Option<SearchHit> {
        let index = line_index(&self.line_starts, at);
        let (line_start, line_end) = line_bounds(&self.content, &self.line_starts, index);
        let line = &self.content[line_start..line_end];

        let hit_start = at - line_start;
        if hit_start + len > line.len() {
            // 查询里带 `\n` 时会走到这里：命中跨行，不属于任何一行。
            return None;
        }
        let hit_end = hit_start + len;
        let matched = &line[hit_start..hit_end];

        let column = utf16_len(&line[..hit_start]);
        let column_end = column + utf16_len(matched);

        // 窗口：命中前最多 SNIPPET_BEFORE 个字符，整体不超过 SNIPPET_CHARS 个字符。
        let mut window_start = hit_start;
        let mut before = 0usize;
        while before < SNIPPET_BEFORE {
            let Some((index, _)) = line[..window_start].char_indices().next_back() else {
                break;
            };
            window_start = index;
            before += 1;
        }

        let mut window_end = hit_end;
        let mut after = 0usize;
        let budget = SNIPPET_CHARS.saturating_sub(SNIPPET_BEFORE + matched.chars().count());
        while after < budget {
            let Some(ch) = line[window_end..].chars().next() else {
                break;
            };
            window_end += ch.len_utf8();
            after += 1;
        }

        Some(SearchHit {
            path: self.path.clone(),
            line: index as u32 + 1,
            text: line[window_start..window_end].to_owned(),
            from: utf16_len(&line[window_start..hit_start]),
            to: utf16_len(&line[window_start..hit_end]),
            column,
            column_end,
            name_hit,
        })
    }
}

/// 一个 Vault 的全文索引（M2 的旧引擎）。
///
/// 构建之后只读，除了 [`SearchIndex::refresh`]——它把"当前正在编辑的那一篇"
/// 从磁盘重读一次，让刚打的字立刻能搜到。索引的其余部分在会话内不变。
///
/// M4 之后这条路径由 `ki-ipc` 的 `Engine::Memory` 开关选择，只用于新旧对拍
/// 与回滚。它仍然完整可用——"索引可重建"这条不变量对它是天然的（本来就在内存里）。
pub struct SearchIndex {
    root: PathBuf,
    files: Vec<NoteLocator>,
    skipped: usize,
    built_ms: u64,
}

impl SearchIndex {
    /// 扫一遍 Vault 并建索引。
    ///
    /// # 为什么复用 `scan_vault`
    ///
    /// 忽略名单（`.git` / `node_modules` / `.knowisle` …）、扩展名、深度上限、
    /// 非 UTF-8 文件名的处理——这些规则**只能有一份**。搜索若自己走一遍
    /// `WalkDir`，两边的"什么算一篇笔记"迟早会不一致，而那种不一致表现为
    /// "树里有这篇、搜不到"，极难排查。
    ///
    /// # 单篇失败不连坐
    ///
    /// 非 UTF-8（B-03）、超大、IO 失败的条目只跳过并计数。一条坏文件让整次
    /// 搜索报错，等于用户放了一个 GBK 文件就再也搜不了整个库。
    pub fn build(root: &Path) -> Result<Self, KiError> {
        let started = std::time::Instant::now();
        let scan = scan_vault(root, &ScanOptions::default())?;
        let root = PathBuf::from(scan.root);

        let mut entries = crate::scan::flatten_files(&scan.tree);
        // 扫描器的顺序是「目录优先 + 名字不区分大小写」；索引只要一个确定性顺序。
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        let mut files = Vec::with_capacity(entries.len());
        let mut skipped = 0usize;
        for entry in entries {
            // 先用扫描时拿到的 size 挡掉超大文件：省一次 open+read。
            // 扫描之后被换掉的文件由 `read_note` 的 10MB 上限兜底。
            if entry.size > MAX_INDEX_BYTES {
                skipped += 1;
                continue;
            }
            match read_note(&root, &entry.path) {
                Ok(note) => files.push(NoteLocator::new(entry.path, note.content)),
                Err(_) => skipped += 1,
            }
        }

        Ok(Self {
            root,
            files,
            skipped,
            built_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// 建索引花了多少毫秒。缓存层用它填 `index_ms`（只有首次搜索非 0）。
    pub fn built_ms(&self) -> u64 {
        self.built_ms
    }

    /// 查询。
    ///
    /// `refresh` 是当前正在编辑的笔记路径：先把它从磁盘重读一次再匹配。
    /// 每次搜索只重读一个文件，换来的是"刚打的字立刻能搜到"——若改成
    /// 整库重建，连打 3 个字符就是 3 次全库 IO；若完全不刷新，
    /// 用户会认定"刚写的搜不到"是 bug（决策 c）。
    pub fn search(&mut self, query: &str, refresh: Option<&str>) -> SearchResult {
        if let Some(path) = refresh {
            self.refresh(path);
        }

        let needle = fold_ascii(query.trim());
        if needle.is_empty() {
            return self.finish(Vec::new(), 0);
        }

        let mut hits: Vec<SearchHit> = Vec::new();
        let mut total = 0usize;

        for file in &self.files {
            let file_hits = file.hits_with_needle(&needle, MAX_HITS_PER_FILE);
            total += file_hits.len();
            hits.extend(file_hits);
        }

        // 文件名命中优先，再按 path、行号。三者都是确定的，所以结果与遍历顺序无关。
        hits.sort_by(|a, b| {
            b.name_hit
                .cmp(&a.name_hit)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });

        self.finish(hits, total)
    }

    fn finish(&self, mut hits: Vec<SearchHit>, total: usize) -> SearchResult {
        let truncated = hits.len() > MAX_HITS;
        hits.truncate(MAX_HITS);
        SearchResult {
            hits,
            total,
            truncated,
            indexed_files: self.files.len(),
            skipped_files: self.skipped,
            // 下面三个字段由缓存层填：索引自己不知道"这次是不是刚建的"，
            // 也不知道后台有没有在跑。
            index_ms: 0,
            builds: 0,
            index_state: IndexState::Idle,
        }
    }

    /// 把一篇笔记从磁盘重读进索引。
    ///
    /// 路径不在索引里（例如扫描之后新建的文件）时什么都不做：
    /// 新文件要等下一次 `vault_scan` 才可见，这是 B-09 的既有边界，
    /// 不在这里偷偷补一个"文件系统监听"。
    ///
    /// 重读失败就把它从索引里摘掉——磁盘上读不到的文件不该继续贡献命中。
    fn refresh(&mut self, path: &str) {
        let Ok(index) = self.files.binary_search_by(|file| file.path().cmp(path)) else {
            return;
        };
        match read_note(&self.root, path) {
            Ok(note) if note.size <= MAX_INDEX_BYTES => {
                self.files[index] = NoteLocator::new(path.to_owned(), note.content);
            }
            _ => {
                self.files.remove(index);
            }
        }
    }
}

/// ASCII 小写折叠。
///
/// 只映射 `A-Z`，其余字符（含全部多字节 UTF-8）原样保留。因此
/// **输出与输入的字节长度逐字节相同**，匹配偏移可以直接用来切原文。
///
/// 刻意不做 Unicode 大小写折叠：`İ` 的小写是 `i̇`（两个码位），长度会变，
/// 那套偏移映射足以写出一个月的 bug；而全角 `Ａ` 与半角 `A` 也不该互相命中
/// （它们是不同的字符，用户输入哪个就该搜哪个）。
pub fn fold_ascii(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect()
}

/// 去掉笔记扩展名之后的文件名。
///
/// 不剥的话，查询 `md` 会让库里**每一篇**都算"文件名命中"，排序立刻失去意义。
pub fn note_stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(index) => {
            let suffix = &name[index..];
            if suffix.eq_ignore_ascii_case(".md") || suffix.eq_ignore_ascii_case(".markdown") {
                &name[..index]
            } else {
                name
            }
        }
        None => name,
    }
}

/// 一个 Vault 相对路径对应的 `name_lower`。
///
/// 这是**唯一**的规则：`ki-store` 的 `note.name_lower` 列与 `NoteLocator`
/// 必须由它算出来。两处各写一遍的话，文件名通道会与定位器对不上，
/// 表现为"文件名命中排在后面"这种没有报错、只是看起来不对劲的 bug。
pub fn name_lower_for_path(path: &str) -> String {
    let name = match path.rsplit_once('/') {
        Some((_, name)) => name,
        None => path,
    };
    fold_ascii(note_stem(name))
}

fn line_starts(content: &str) -> Vec<u32> {
    let mut starts = Vec::with_capacity(16);
    starts.push(0);
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            starts.push((index + 1) as u32);
        }
    }
    starts
}

/// `at` 落在第几行（0-based）。`starts[0] == 0`，所以结果恒 ≥ 0。
fn line_index(starts: &[u32], at: usize) -> usize {
    starts.partition_point(|&start| start as usize <= at) - 1
}

/// 第 `index` 行的字节区间 `[start, end)`，已剥掉行尾的 `\r`。
fn line_bounds(content: &str, starts: &[u32], index: usize) -> (usize, usize) {
    let start = starts[index] as usize;
    let mut end = match starts.get(index + 1) {
        Some(&next) => next as usize - 1,
        None => content.len(),
    };
    if end > start && content.as_bytes()[end - 1] == b'\r' {
        end -= 1;
    }
    (start, end)
}

fn utf16_len(text: &str) -> u32 {
    text.chars().map(|c| c.len_utf16() as u32).sum()
}
