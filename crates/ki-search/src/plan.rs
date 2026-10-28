//! 查询计划：把用户输入翻译成「走哪条通道 + 什么 FTS5 表达式」。
//!
//! # 为什么必须有一个独立的计划阶段
//!
//! FTS5 的 `MATCH` 接收的是**它自己的查询语言**，不是字符串。用户输入
//! `-x`、`OR`、`NEAR(`、`"`、`*` 都会让 `MATCH` 抛语法错误——那是一条
//! 从「搜索框」直通「Rust 错误弹窗」的路径。所以用户输入**永远不能**原样
//! 拼进表达式：必须先过这里，逐 token 加引号，或者干脆判定「走全扫」。
//!
//! # 四种结果
//!
//! | 模式 | 何时 | 谁干活 |
//! |---|---|---|
//! | [`Mode::Empty`] | 空查询 / 含换行 | 直接返回空结果 |
//! | [`Mode::FullScan`] | ASCII 单字、纯标点、emoji、含 CJK 但切不出 token | 逐篇字面扫描 |
//! | [`Mode::Cjk`] | 查询里有 CJK 且切得出 token（**含单个 CJK 字**） | `note_fts_zh` |
//! | [`Mode::Ascii`] | 查询里只有 ASCII，且（多词 或 单词单字符） | `note_fts`（词首前缀） |
//! | [`Mode::AsciiInfix`] | 查询是**单个** ≥2 字符的 ASCII 词 | `note_fts_ascii_bi`（词内 bigram） |
//!
//! # 全扫不是兜底，是正确性要求
//!
//! 索引里**根本没有可查的东西**时，硬拿空表达式去 MATCH 会抛错
//! （`MATCH ''` 是语法错误，`MATCH '""'` 才是空集），而返回空集又是错的
//! （库里确实有 `🙂` 这个字）。所以只有两条路：全扫，或者撒谎。选全扫。
//!
//! 今天还留在全扫这一列的只有三类：ASCII 单字（ASCII 投影是**词首对齐**的，
//! `"a"*` 查不到 `Rust` 里的 `a`）、纯 emoji / 纯标点（没有 token）、
//! 以及 `🙂的` / `A 的` 这种「有 CJK 但切不出 token」的查询。
//!
//! # 子串查询为什么不是「一种新查询类型」
//!
//! [`Mode::AsciiInfix`] 只影响**走哪张表 + 什么表达式**：它和 `Ascii` 共用
//! `indexed_search` 的同一条流水线（一次 MATCH → 文件名通道 → RRF → 池子
//! 预算 → 定位器），只是多一个 `match_*` 调用。没有第二条融合路径、没有
//! 第二套池子规则、没有第二个 `total` 语义。
//!
//! 范围刻意收窄：**只接管「单个 ≥2 字符的 ASCII 词」**。多词查询
//! （`rust-ownership`）仍走词首前缀——把每个词都换成子串会改变它们的候选集，
//! 而那不是本刀要修的东西（`rust` 是 `rust-ownership` 的词首前缀，本来就能中）。
//!
//! # CJK 单字为什么能走索引
//!
//! 它以前也在全扫那一列：没有 bigram，索引里查不到。但索引侧给每个 CJK run
//! 补了末字符的单字 token（见 [`crate::bigram::index_tokens`]），于是
//! 「字出现在正文任何位置 ⇒ 至少有一个以它开头的 token」成立，`"记"*`
//! 就是一条合法的 FTS5 前缀查询。
//!
//! **单字不是一种新的查询类型**：它产出的仍是 `Mode::Cjk` + 一个表达式，
//! 下游的 `indexed_search` 一行都不用改。多一条分支就多一处会与主路分叉的
//! 地方，而分叉的那一条永远是最少被跑到的。

use crate::{ascii, bigram};

/// 查询要走哪条通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 空查询（或含换行）：没有命中可言。
    Empty,
    /// 索引里没有可查的 token，只能逐篇字面扫描。
    FullScan,
    /// 通道 A：ASCII 词的前缀匹配。
    Ascii,
    /// 通道 A'：**单个** ≥2 字符 ASCII 词的词内 bigram（子串语义）。
    AsciiInfix,
    /// 通道 B：CJK bigram 的 AND；单字查询是一条前缀（`"记"*`）。
    Cjk,
}

/// CJK token 的截断上限。
///
/// 一个 64 字的查询会产生 63 个 bigram，拼出来的表达式已经接近 SQLite
/// 的解析舒适区。截断**只放宽候选**（少 AND 几个条件），不会漏召回——
/// 定位层仍然拿完整原串做字面判定。
pub const MAX_CJK_TOKENS: usize = 64;

/// 子串通道的 bigram 上限。理由同上：截断只放宽候选，不漏召回。
///
/// 一个 200 字符的「词」（base64 串、压缩过的路径）会产出 199 个 bigram，
/// 拼出来是一条约 1.2 KB 的表达式。64 个足够把任何真实标识符钉死。
pub const MAX_INFIX_BIGRAMS: usize = 64;

/// ASCII 词的截断上限。理由同上。
pub const MAX_ASCII_WORDS: usize = 16;

/// 一份查询计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub mode: Mode,
    /// 交给 `MATCH` 的表达式。`Empty` / `FullScan` 时为空串。
    pub expr: String,
    /// 投影出来的 ASCII 词（小写，已去重、已截断）。诊断与测试用。
    pub ascii_words: Vec<String>,
}

impl Plan {
    fn bare(mode: Mode) -> Self {
        Self {
            mode,
            expr: String::new(),
            ascii_words: Vec::new(),
        }
    }

    /// 是否需要真的去查 FTS。`false` 表示由调用方决定「全扫」或「直接返回空」。
    pub fn needs_match(&self) -> bool {
        matches!(self.mode, Mode::Ascii | Mode::AsciiInfix | Mode::Cjk)
    }
}

/// 生成查询计划。
pub fn plan(query: &str) -> Plan {
    let needle = query.trim();

    // 1. 空查询。
    if needle.is_empty() {
        return Plan::bare(Mode::Empty);
    }

    // 2. 含换行。旧引擎对这种查询的跨行命中一律跳过（没有「哪一行」可言），
    //    所以命中数恒为 0。直接返回 Empty 是**保持契约**，不是优化。
    //    （正常输入框打不出换行，但 IPC 可以被直接调用。）
    if needle.contains(['\n', '\r']) {
        return Plan::bare(Mode::Empty);
    }

    // 3. 单字符。分两类：
    //    - CJK 单字：索引里**有**以它开头的 token（run 内部 bigram 的首字，
    //      以及每个 run 末字符的覆盖 token），所以 `"c"*` 是一条合法的前缀
    //      查询，走索引而不是全扫。
    //    - ASCII 单字 / 纯标点 / emoji：索引里查不到。ASCII 投影是**词首
    //      对齐**的，`"a"*` 会漏掉 `Rust` 里的 `a`；emoji 根本没有 token。
    //      全库找单个字符是有意义的（`🙂`），所以全扫而不是返回空。
    if needle.chars().count() == 1 {
        let c = needle.chars().next().expect("上面刚判过非空");
        if bigram::is_cjk(c) {
            return Plan {
                mode: Mode::Cjk,
                // 星号必须在引号**外**：`"记"*` 是前缀，`"记*"` 是字面 token。
                expr: format!("{}*", quote(&c.to_string())),
                ascii_words: Vec::new(),
            };
        }
        return Plan::bare(Mode::FullScan);
    }

    // 4. CJK bigram。
    let cjk_tokens = dedup_truncate(bigram::tokens(needle), MAX_CJK_TOKENS);
    if !cjk_tokens.is_empty() {
        return Plan {
            mode: Mode::Cjk,
            expr: and_expr(cjk_tokens.iter().map(|t| quote(t))),
            ascii_words: Vec::new(),
        };
    }

    // 5. 含 CJK 但切不出 bigram（`🙂的`、`A 的`）：索引里没有它的 token。
    if needle.chars().any(bigram::is_cjk) {
        return Plan::bare(Mode::FullScan);
    }

    // 6. 纯 ASCII。
    //
    //    注意这里**不**把 ASCII 词与 CJK bigram AND 在一起（第 4 步就直接返回了）：
    //    `xRust的所有权` 的 ASCII 投影是 `xrust`，再加 `"rust"*` 会造成假阴性
    //    （文档里是 `Rust`，前缀 `xrust` 不匹配）。CJK bigram 已经足够窄，
    //    ASCII 部分交给定位器过滤。
    let projected = ascii::project(needle);
    let words: Vec<String> = projected.split_whitespace().map(str::to_owned).collect();

    // 6a. 单个 ≥2 字符的词 → 词内 bigram（子串语义）。
    //
    //     `us` 在 `rust` 的**中间**，词首前缀永远够不着它；`"us"` 这个 token
    //     却在 `body_bigram` 里（`rust` → `ru us st`）。召回证明见
    //     [`ascii::bigrams`]。
    //
    //     只有**单个**词才接管：多词查询（`rust-ownership`）的每个词今天都是
    //     词首前缀，换成子串会改变它们的候选集，而那不在本刀范围内。
    if words.len() == 1 && words[0].chars().count() >= 2 {
        let pairs = dedup_truncate(
            ascii::bigrams(&projected)
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
            MAX_INFIX_BIGRAMS,
        );
        // 长度 ≥2 的词至少产出一个 bigram；真出了空集说明 `bigrams` 坏了，
        // 而拿空表达式去 MATCH 会抛 `fts5: syntax error`。
        debug_assert!(!pairs.is_empty(), "{:?} 没有 bigram", words[0]);
        if !pairs.is_empty() {
            return Plan {
                mode: Mode::AsciiInfix,
                // 子串查询**不带**星号：`"us"` 是精确 token，`"us"*` 会多出
                // `user`/`usb` 这类以 `us` 开头的词——那是前缀语义，不是子串。
                expr: and_expr(pairs.iter().map(|p| quote(p))),
                ascii_words: words,
            };
        }
    }

    // 6b. 多词，或单词但只有一个字符 → 词首前缀（与改前逐字相同）。
    let words = dedup_truncate(words, MAX_ASCII_WORDS);
    if !words.is_empty() {
        return Plan {
            mode: Mode::Ascii,
            // 星号必须在引号**外**：`"rust"*` 是前缀，`"rust*"` 是字面 token。
            expr: and_expr(words.iter().map(|w| format!("{}*", quote(w)))),
            ascii_words: words,
        };
    }

    // 7. 纯标点 / 纯 emoji：没有可查的 token，但字面全扫是有意义的。
    Plan::bare(Mode::FullScan)
}

/// FTS5 字符串字面量：双引号包裹，内部双引号双写。
///
/// 这一步之后，token 里的 `-`、`OR`、`*`、`(` 都变成普通字符，
/// 不再被当成运算符。
fn quote(token: &str) -> String {
    let mut out = String::with_capacity(token.len() + 2);
    out.push('"');
    for c in token.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn and_expr(parts: impl Iterator<Item = String>) -> String {
    parts.collect::<Vec<_>>().join(" AND ")
}

/// 去重并截断，保留首次出现的顺序。
///
/// 去重是安全的：AND 是幂等的，`"知识" AND "知识"` 与 `"知识"` 等价。
/// 不去重的话，`知识库知识库知识库` 这种输入会拼出三倍长的表达式。
fn dedup_truncate(items: Vec<String>, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len().min(max));
    for item in items {
        if out.len() >= max {
            break;
        }
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_whitespace_queries_are_empty() {
        for q in ["", "   ", "\t", "\u{3000}"] {
            assert_eq!(plan(q).mode, Mode::Empty, "{q:?}");
        }
    }

    /// 含换行的查询在旧引擎里命中数恒为 0，这里必须一致。
    #[test]
    fn newline_queries_are_empty() {
        assert_eq!(plan("a\nb").mode, Mode::Empty);
        assert_eq!(plan("a\r\nb").mode, Mode::Empty);
        // 首尾换行被 trim 掉之后就不含换行了
        assert_eq!(plan("\nrust\n").mode, Mode::AsciiInfix);
    }

    /// CJK 单字走前缀通道：`"c"*`，模式仍是 `Cjk`（不是新查询类型）。
    ///
    /// 这条的依据在索引侧：`index_tokens` 给每个 run 补了末字符的单字 token，
    /// 所以「字出现在正文任何位置」都能被前缀查到。见
    /// `bigram::every_cjk_char_has_a_token_starting_with_it`。
    #[test]
    fn single_cjk_char_uses_the_prefix_channel() {
        for (query, expr) in [
            ("的", r#""的"*"#),
            ("记", r#""记"*"#),
            ("屿", r#""屿"*"#),
            // 假名、谚文也在 `is_cjk` 里，走同一条路。
            ("あ", r#""あ"*"#),
            ("한", r#""한"*"#),
        ] {
            let p = plan(query);
            assert_eq!(p.mode, Mode::Cjk, "{query:?}");
            assert_eq!(p.expr, expr, "{query:?}");
            assert!(p.needs_match(), "{query:?}");
        }
    }

    /// ASCII 单字、纯标点、emoji 仍然全扫——索引里没有它们的 token。
    #[test]
    fn single_ascii_punctuation_and_emoji_still_full_scan() {
        for query in ["a", "Z", "1", "🙂", "，", "—"] {
            let p = plan(query);
            assert_eq!(p.mode, Mode::FullScan, "{query:?}");
            assert!(!p.needs_match(), "{query:?}");
        }
    }

    #[test]
    fn cjk_query_builds_an_and_of_bigrams() {
        let p = plan("知识库");
        assert_eq!(p.mode, Mode::Cjk);
        assert_eq!(p.expr, r#""知识" AND "识库""#);
        assert!(p.needs_match());
    }

    #[test]
    fn ascii_query_builds_prefix_terms() {
        let p = plan("Rust Book");
        assert_eq!(p.mode, Mode::Ascii);
        assert_eq!(p.expr, r#""rust"* AND "book"*"#);
        assert_eq!(p.ascii_words, ["rust", "book"]);
    }

    /// 单个 ≥2 字符的 ASCII 词走**子串**通道：词内 bigram 的 AND，不带星号。
    ///
    /// `us` 是那个经典例子：它是 `rust` 的中间两个字符，词首前缀永远够不着。
    #[test]
    fn single_ascii_word_uses_the_infix_channel() {
        for (query, expr) in [
            ("us", r#""us""#),
            ("ow", r#""ow""#),
            ("ru", r#""ru""#),
            ("rus", r#""ru" AND "us""#),
            ("rust", r#""ru" AND "us" AND "st""#),
            ("RUST", r#""ru" AND "us" AND "st""#),
            ("api", r#""ap" AND "pi""#),
        ] {
            let p = plan(query);
            assert_eq!(p.mode, Mode::AsciiInfix, "{query:?}");
            assert_eq!(p.expr, expr, "{query:?}");
            assert!(p.needs_match(), "{query:?}");
            assert!(!p.expr.contains('*'), "{query:?} 子串查询不许带星号");
        }
    }

    /// 子串通道**只**接管「单个词」。多词查询的每个词今天都是词首前缀，
    /// 换成子串会改变它们的候选集——`rust` 是 `rust-ownership` 的前缀，本来就能中。
    #[test]
    fn multi_word_ascii_stays_on_the_prefix_channel() {
        for query in ["rust book", "rust-ownership", "snake_case", "a b", "e_c"] {
            let p = plan(query);
            assert_eq!(p.mode, Mode::Ascii, "{query:?}");
            assert!(
                p.expr.split(" AND ").all(|part| part.ends_with("\"*")),
                "{query:?} → {}",
                p.expr
            );
        }
    }

    /// 星号必须在引号外。写进引号里会被当成字面 token（`rust*` 找不到 `rust`）。
    #[test]
    fn prefix_star_is_outside_the_quotes() {
        let p = plan("rust book");
        assert!(p.expr.ends_with("\"*"), "{}", p.expr);
        assert!(!p.expr.contains("*\""));
    }

    /// 超长的「词」截断 bigram：只放宽候选，不改变模式。
    ///
    /// 构造一个有 71 个**互不相同** bigram 的 72 字符词：第 `i` 个字符对是
    /// `(i, i+18) mod 36`，词内相邻对差 18，跨对边界差 1——两组不可能相等。
    /// 这样「截到 64 个」才是一个可断言的数，而不是「大概会截断」。
    #[test]
    fn long_infix_words_are_truncated() {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let long: String = (0..36)
            .flat_map(|i| [ALPHABET[i] as char, ALPHABET[(i + 18) % 36] as char])
            .collect();
        let p = plan(&long);
        assert_eq!(p.mode, Mode::AsciiInfix);
        assert_eq!(p.expr.split(" AND ").count(), MAX_INFIX_BIGRAMS);
        // 截断保留前 64 个，所以第一个分量必须是词的首个 bigram。
        assert_eq!(p.expr.split(" AND ").next(), Some(r#""as""#));
    }

    /// 含 CJK 但切不出 bigram 时必须全扫，不能拿空表达式去 MATCH。
    #[test]
    fn cjk_without_bigrams_goes_full_scan() {
        assert_eq!(plan("🙂的").mode, Mode::FullScan);
        assert_eq!(plan("A 的").mode, Mode::FullScan);
        assert_eq!(plan("的 的").mode, Mode::FullScan);
    }

    /// 纯标点 / 纯 emoji 也要全扫：库里可能真有这些字符。
    #[test]
    fn punctuation_only_goes_full_scan() {
        assert_eq!(plan("---").mode, Mode::FullScan);
        assert_eq!(plan("🙂🙂").mode, Mode::FullScan);
        assert_eq!(plan("，。").mode, Mode::FullScan);
    }

    /// FTS5 的运算符必须被引号吃掉，否则 `MATCH` 直接抛语法错误。
    #[test]
    fn fts_operators_are_neutralized() {
        for q in ["OR", "NEAR", "-x", "a OR b", "NOT c", "*"] {
            let p = plan(q);
            assert_ne!(p.mode, Mode::Cjk);
            if p.needs_match() {
                // 每个 AND 的分量都必须是 `"..."` 或 `"..."*`
                for part in p.expr.split(" AND ") {
                    assert!(part.starts_with('"'), "{q:?} → {}", p.expr);
                }
            }
        }
    }

    /// 引号本身要双写，否则表达式会被提前截断。
    ///
    /// ASCII 侧其实走不到这里（[`ascii::project`] 已经把引号抹成空格），
    /// 但 `quote` 是防御性的第二道：哪天有人给通道 B 加了别的 token 来源，
    /// 这里仍然得对。
    #[test]
    fn embedded_quotes_are_doubled() {
        assert_eq!(quote(r#"a"b"#), r#""a""b""#);
        assert_eq!(quote("知识"), r#""知识""#);
        assert_eq!(quote(""), r#""""#);
    }

    /// 长查询截断只放宽候选，不改变模式。
    #[test]
    fn long_queries_are_truncated() {
        // 80 个互不相同的汉字 → 79 个互不相同的 bigram，必然超过上限。
        let long: String = (0..80u32)
            .map(|i| char::from_u32(0x4E00 + i).expect("CJK 基本区"))
            .collect();
        let p = plan(&long);
        assert_eq!(p.mode, Mode::Cjk);
        assert_eq!(p.expr.split(" AND ").count(), MAX_CJK_TOKENS);

        let many_words: String = (0..40)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let p = plan(&many_words);
        assert_eq!(p.mode, Mode::Ascii);
        assert_eq!(p.ascii_words.len(), MAX_ASCII_WORDS);
    }

    /// 重复 token 去重：`知识库知识库知识库` 不该拼出三倍长的表达式。
    #[test]
    fn duplicate_tokens_are_deduped() {
        let p = plan("知识库知识库知识库");
        let parts: Vec<&str> = p.expr.split(" AND ").collect();
        let mut unique = parts.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(parts.len(), unique.len(), "{}", p.expr);
    }

    /// 全扫与空计划都不带表达式——调用方靠 `mode` 分流，不该去读 `expr`。
    #[test]
    fn non_match_modes_have_empty_expr() {
        for q in ["", "a", "🙂🙂", "🙂的"] {
            let p = plan(q);
            assert!(p.expr.is_empty(), "{q:?}");
            assert!(!p.needs_match());
        }
    }
}
