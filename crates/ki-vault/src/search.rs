//! 进程内全文索引（M2 第三刀）。
//!
//! # 这不是搜索引擎
//!
//! 匹配规则只有一条：**ASCII 大小写折叠之后的子串匹配**。不模糊、不分词、
//! 不支持正则。中文天然可用——UTF-8 的子串匹配对中文是精确的，代价是没有任何
//! 相关性排序（结果只按「文件名命中 > 路径 > 行号」排）。
//!
//! 为什么不做模糊：用户按 `Cmd+Shift+F` 时想找的是"哪篇笔记写了这句话"，
//! 不是"哪篇笔记的字母顺序最接近"。模糊匹配在文件名上是对的（见 `quick-open`），
//! 在正文上只会把噪音排到前面。
//!
//! # 为什么只在内存里
//!
//! 磁盘上的 Markdown 是唯一真相源（公理 A1）。没有 watcher（B-09）时，
//! 持久化索引**必然**与磁盘漂移，而且漂移是不可见的：用户改了文件，
//! 搜出来的还是旧句子，没有任何提示。内存索引把这份风险限制在"本次会话"，
//! 重新扫描（`vault_scan`）就丢缓存，语义干净。
//!
//! 代价写在明处：
//!
//! - 冷启动要读一遍全库（1000 篇实测 100–200ms），首次搜索慢；
//! - 外部改动在重新扫描之前搜不到（B-09 的既有缺口，不是本模块引入的）；
//! - 内存约为库内正文的 **两倍**（原文 + ASCII 小写副本）。
//!
//! M3 会用 `ki-search` 的 FTS5 索引替换本模块。届时"索引可重建"这条不变量
//! 必须原样保留：删掉索引文件，功能仍然完整，只是第一次搜索变慢。
//!
//! # 索引长什么样
//!
//! 每篇笔记存两份字符串：原文（切片用）与小写副本（匹配用）。
//! 小写化只动 ASCII 字母，因此**两份的字节长度逐字节相同**——
//! 匹配到的字节偏移可以直接拿去切原文，不需要任何偏移换算。
//! 这是"只折叠 ASCII"这个决定带来的第二个好处（第一个是中文天然可用）。

use std::path::{Path, PathBuf};

use ki_core::{FileNode, KiError, SearchHit, SearchResult};

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

/// 一篇进了索引的笔记。
struct IndexedFile {
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

impl IndexedFile {
    fn new(path: String, content: String) -> Self {
        let name = match path.rsplit_once('/') {
            Some((_, name)) => name,
            None => path.as_str(),
        };
        let name_lower = fold_ascii(note_stem(name));
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

    /// 命中位置所在行的下一行起始偏移。用来保证"一行只出一条命中"。
    fn next_line_start(&self, at: usize) -> usize {
        let index = line_index(&self.line_starts, at);
        match self.line_starts.get(index + 1) {
            Some(&next) => next as usize,
            None => self.content.len(),
        }
    }
}

/// 一个 Vault 的全文索引。
///
/// 构建之后只读，除了 [`SearchIndex::refresh`]——它把"当前正在编辑的那一篇"
/// 从磁盘重读一次，让刚打的字立刻能搜到。索引的其余部分在会话内不变。
pub struct SearchIndex {
    root: PathBuf,
    files: Vec<IndexedFile>,
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

        let mut entries: Vec<(String, u64)> = Vec::new();
        collect_files(&scan.tree, &mut entries);
        // 扫描器的顺序是「目录优先 + 名字不区分大小写」；索引只要一个确定性顺序。
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut files = Vec::with_capacity(entries.len());
        let mut skipped = 0usize;
        for (path, size) in entries {
            // 先用扫描时拿到的 size 挡掉超大文件：省一次 open+read。
            // 扫描之后被换掉的文件由 `read_note` 的 10MB 上限兜底。
            if size > MAX_INDEX_BYTES {
                skipped += 1;
                continue;
            }
            match read_note(&root, &path) {
                Ok(note) => files.push(IndexedFile::new(path, note.content)),
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
            let name_hit = file.name_lower.contains(&needle);
            let mut cursor = 0usize;
            let mut used = 0usize;

            while used < MAX_HITS_PER_FILE {
                let Some(offset) = file.lower[cursor..].find(&needle) else {
                    break;
                };
                let at = cursor + offset;
                // 查询里带换行（正常输入框打不出来，但 IPC 可以被直接调用）时，
                // 命中可能横跨两行。那种命中没有"哪一行"可言，跳过而不是硬切。
                if let Some(hit) = make_hit(file, at, needle.len(), name_hit) {
                    hits.push(hit);
                    total += 1;
                    used += 1;
                }
                // 同一行的后续命中不再单独成条：跳到下一行。
                cursor = file.next_line_start(at);
                if cursor >= file.content.len() {
                    break;
                }
            }
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
            // 下面两个字段由缓存层填：索引自己不知道"这次是不是刚建的"。
            index_ms: 0,
            builds: 0,
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
        let Ok(index) = self
            .files
            .binary_search_by(|file| file.path.as_str().cmp(path))
        else {
            return;
        };
        match read_note(&self.root, path) {
            Ok(note) if note.size <= MAX_INDEX_BYTES => {
                self.files[index] = IndexedFile::new(path.to_owned(), note.content);
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
fn fold_ascii(input: &str) -> String {
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

/// 组装一条命中；命中横跨两行时返回 `None`。
///
/// `at` 是命中在**小写副本**里的字节偏移；因为折叠不改变字节长度，
/// 它同时是原文里的偏移。
fn make_hit(file: &IndexedFile, at: usize, len: usize, name_hit: bool) -> Option<SearchHit> {
    let index = line_index(&file.line_starts, at);
    let (line_start, line_end) = line_bounds(&file.content, &file.line_starts, index);
    let line = &file.content[line_start..line_end];

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
        path: file.path.clone(),
        line: index as u32 + 1,
        text: line[window_start..window_end].to_owned(),
        from: utf16_len(&line[window_start..hit_start]),
        to: utf16_len(&line[window_start..hit_end]),
        column,
        column_end,
        name_hit,
    })
}

/// 去掉笔记扩展名之后的文件名。
///
/// 不剥的话，查询 `md` 会让库里**每一篇**都算"文件名命中"，排序立刻失去意义。
fn note_stem(name: &str) -> &str {
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

/// 把树压成 `(相对路径, 字节数)`。用显式栈，与扫描器的深度上限无关。
fn collect_files(tree: &FileNode, out: &mut Vec<(String, u64)>) {
    let mut stack = vec![tree];
    while let Some(node) = stack.pop() {
        if let Some(children) = &node.children {
            for child in children {
                stack.push(child);
            }
        } else {
            out.push((node.path.clone(), node.size));
        }
    }
}
