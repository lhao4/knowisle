//! 通道 B 的切分：CJK bigram + 跨脚本边界 bigram。
//!
//! # 为什么是 bigram 而不是分词
//!
//! 索引层的任务只有一条：**不能漏候选**。定位层（`ki_vault::NoteLocator`）
//! 会拿原查询逐行做字面子串判定，所以索引只需要保证「真命中的文档一定进候选池」。
//!
//! bigram 恰好能给出这条保证，而且是可以证明的：
//!
//! > 若 `Q ⊆ T`（Q 是 T 的连续子串）且 Q 含 ≥2 个汉字，
//! > 则 Q 的 bigram 多重集 ⊆ T 的 bigram 多重集。
//!
//! 证明是一行：Q 里每个 bigram 都是 T 里同一个位置的 bigram。多重集而非集合，
//! 是因为 `哈哈` 在 T 里出现一次、在 Q 里出现两次时，`哈` 需要能被用两次——
//! 而 FTS5 的 AND 只查「有没有」，比多重集更宽松，所以**这条保证只会更强**。
//!
//! 反过来，jieba 这类分词给不出任何保证：查询切出来的词与文档切出来的词
//! 依赖上下文，`知识库` 在一处成词、在另一处被切成 `知识` + `库`，
//! 于是「明明有这句话却搜不到」。它换来的是更好的相关性排序——但排序在
//! 我们这里是 RRF 与定位器的事，不该拿召回率去换。
//!
//! 代价写在明处：索引比词表方案大（每字约 2 个 token），且**单字查询没有
//! bigram**，必须走全扫（见 [`crate::plan`]）。

/// 字符是否属于 CJK 书写系统。
///
/// 覆盖 Han（含扩展区与兼容区）、假名、谚文、注音。**不含** CJK 标点
/// （`U+3000..=U+303F`）与全角形式（`U+FF00..=U+FFEF`）——
/// 前者是分隔符，后者里的全角字母数字属于 `Word`（由通道 A 负责）。
///
/// 唯一的例外是 `々`（`U+3005`，日文叠字符）：它出现在词的**内部**
/// （`時々`），当成分隔符会把词切断，所以归进 CJK。
pub fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3005          // 々 叠字符，词内字符
            | 0x2E80..=0x2EFF // CJK 部首补充
            | 0x3040..=0x309F // 平假名
            | 0x30A0..=0x30FF // 片假名
            | 0x3100..=0x312F // 注音
            | 0x3130..=0x318F // 谚文兼容字母
            | 0x31A0..=0x31BF // 注音扩展
            | 0x31F0..=0x31FF // 片假名语音扩展
            | 0x3400..=0x4DBF // CJK 扩展 A
            | 0x4E00..=0x9FFF // CJK 基本区
            | 0xA960..=0xA97F // 谚文字母扩展 A
            | 0xAC00..=0xD7AF // 谚文音节
            | 0xF900..=0xFAFF // CJK 兼容汉字
            | 0x1B000..=0x1B16F // 假名补充 / 扩展
            | 0x20000..=0x2A6DF // CJK 扩展 B
            | 0x2A700..=0x2EBEF // CJK 扩展 C–F
            | 0x2F800..=0x2FA1F // CJK 兼容汉字补充
            | 0x30000..=0x3134F // CJK 扩展 G
    )
}

/// 「词字符」：字母或数字，但不是 CJK。
///
/// 这是跨脚本边界 bigram 的判定条件。`_`、`-`、emoji 都不算——
/// 它们不是词的一部分，`foo_bar` 在语义上是两个词。
fn is_word(c: char) -> bool {
    c.is_alphanumeric() && !is_cjk(c)
}

fn pair(a: char, b: char) -> String {
    let mut s = String::with_capacity(a.len_utf8() + b.len_utf8());
    s.push(a);
    s.push(b);
    s
}

/// 一段正文的通道 B token 列表。
///
/// 顺序是确定的：先按 CJK run 从左到右，每个 run 内部先发全部重叠 bigram，
/// 再发前边界、后边界。**保留重复**——bm25 需要词频，去重会把它变成布尔检索。
///
/// # 规则
///
/// 1. 切成极大的 CJK run 与 Word run，其余字符是分隔。
/// 2. 每个 CJK run：长度 ≥2 时发出全部重叠 bigram；长度 1 不发内部 bigram。
/// 3. 边界 bigram：run 前一个字符是 `Word` → 发 `prev + run[0]`；
///    run 后一个字符是 `Word` → 发 `run[last] + next`。
/// 4. Word run 本身不进通道 B（通道 A 的 ASCII 投影负责）。
///
/// 第 3 条是必须的：`知识库Rust` 里 `库R` 是唯一的跨脚本线索，不发它的话
/// 查询 `库R` 会漏召回。反过来 `Rust的所有权` 里的 `t的` 同理。
pub fn tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if !is_cjk(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_cjk(chars[i]) {
            i += 1;
        }
        let run = &chars[start..i];

        for w in run.windows(2) {
            out.push(pair(w[0], w[1]));
        }
        if start > 0 && is_word(chars[start - 1]) {
            out.push(pair(chars[start - 1], run[0]));
        }
        if i < chars.len() && is_word(chars[i]) {
            out.push(pair(run[run.len() - 1], chars[i]));
        }
    }

    out
}

/// 把 [`tokens`] 的结果拼成写进 FTS 列的字符串（单空格连接）。
///
/// 空列表返回空串。**空串不能拿去 MATCH**：`MATCH ''` 是语法错误
/// （`MATCH '""'` 才是空集），所以调用方必须先判空。
pub fn token_stream(text: &str) -> String {
    tokens(text).join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<&str> {
        // 测试里借用返回的 String 不方便，这里返回一份泄漏的切片即可。
        Box::leak(token_stream(s).into_boxed_str())
            .split_whitespace()
            .collect()
    }

    /// 方案里逐个钉死的例子。改规则必须先改这张表。
    #[test]
    fn segmentation_matches_the_spec_table() {
        assert_eq!(toks("知识库"), ["知识", "识库"]);
        assert_eq!(toks("Rust的所有权"), ["的所", "所有", "有权", "t的"]);
        assert_eq!(toks("知识库Rust"), ["知识", "识库", "库R"]);
        assert_eq!(toks("的"), Vec::<&str>::new());
        assert_eq!(toks("🙂🙂"), Vec::<&str>::new());
        assert_eq!(toks("foo_bar"), Vec::<&str>::new());
    }

    /// 单字 CJK 不发内部 bigram，但相邻的 Word 仍然能给出边界 bigram。
    #[test]
    fn single_cjk_char_only_yields_boundary_bigrams() {
        assert!(tokens("的").is_empty());
        assert_eq!(toks("A的"), ["A的"]);
        assert_eq!(toks("的A"), ["的A"]);
        // 空格把两侧都隔开了，没有边界 bigram 可言
        assert!(tokens("A 的").is_empty());
    }

    /// 纯标点、emoji、下划线、连字符都不产生 token。
    #[test]
    fn punctuation_and_symbols_are_separators() {
        for text in ["，。！？", "---", "___", "①②③", "\u{3000}\u{3000}", ""] {
            assert!(tokens(text).is_empty(), "{text:?} 不该产生 token");
        }
    }

    /// CJK 标点不在 CJK 区块里：`你好，世界` 必须是两个 run，不能跨标点发 bigram。
    #[test]
    fn cjk_punctuation_breaks_the_run() {
        assert_eq!(toks("你好，世界"), ["你好", "世界"]);
    }

    /// 全角字母数字属于 `Word`（由通道 A 负责），不是 CJK。
    #[test]
    fn fullwidth_alphanumerics_are_word_chars() {
        assert!(!is_cjk('Ａ'));
        assert!(!is_cjk('１'));
        assert_eq!(toks("Ａ的"), ["Ａ的"]);
    }

    /// 重复必须保留：`哈哈` 在 FTS 里的词频要能被 bm25 看见。
    #[test]
    fn duplicates_are_preserved() {
        assert_eq!(toks("哈哈"), ["哈哈"]);
        assert_eq!(toks("哈哈哈"), ["哈哈", "哈哈"]);
    }

    /// 三种文字的 run 各自独立，跨脚本边界各发一次。
    #[test]
    fn mixed_scripts_get_boundary_bigrams_on_both_sides() {
        // 汉字 run 左右各贴一个 Word run
        assert_eq!(toks("a知识b"), ["知识", "a知", "识b"]);
    }

    /// 完备性：随机中文子串的 bigram 多重集必然被父串覆盖。
    ///
    /// 这是「bigram 不漏召回」的可执行形式。用自带的 xorshift 而不是引 `rand`，
    /// 是为了让这个 crate 保持零依赖。
    #[test]
    fn random_cjk_substring_has_all_its_bigrams_in_the_parent() {
        let mut rng = XorShift::new(0x9E37_79B9_7F4A_7C15);
        let alphabet: Vec<char> = "知识库的设计实现原语抽象岛屿边界检索索引分词投影"
            .chars()
            .collect();

        for _ in 0..300 {
            let len = 8 + rng.below(40);
            let parent: String = (0..len)
                .map(|_| alphabet[rng.below(alphabet.len())])
                .collect();
            let parent_tokens = tokens(&parent);

            // 从父串里随机切一段连续子串（≥2 字）
            let chars: Vec<char> = parent.chars().collect();
            // len >= 8，所以 start <= len-2，切片必然拿到至少 2 个字符。
            let start = rng.below(chars.len() - 1);
            let take = 2 + rng.below(chars.len() - start - 1);
            let query: String = chars[start..start + take].iter().collect();

            for token in tokens(&query) {
                assert!(
                    parent_tokens.contains(&token),
                    "漏召回：父串 {parent:?} 里找不到子串 {query:?} 的 bigram {token:?}"
                );
            }
        }
    }

    /// 自带的伪随机数发生器（xorshift64*）。只为测试可复现。
    struct XorShift(u64);

    impl XorShift {
        fn new(seed: u64) -> Self {
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
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                return 0;
            }
            (self.next() % n as u64) as usize
        }
    }
}
