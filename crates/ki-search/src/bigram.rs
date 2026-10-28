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
//! 代价写在明处：索引比词表方案大（每字约 2 个 token）。
//!
//! # 查询侧与索引侧不是同一份 token
//!
//! [`tokens`] 是**查询侧**：只有 bigram（内部 + 跨脚本边界）。它成立的依据
//! 是上面那条证明——AND 起来的每个分量都必须满足「只要子串在，它就在」。
//!
//! [`index_tokens`] 是**索引侧**：在查询侧之上，给每个 CJK run 补一个
//! **末字符的单字 token**。这条补充**只对索引成立**，理由是覆盖：
//!
//! > run 里位置 `0..n-2` 的字符都是某个 bigram 的首字，末字符 `n-1` 不是。
//! > 补一个末字符的单字 token，就有「字出现在正文任何位置 ⇒ 至少有一个
//! > 以它开头的 token」。
//!
//! 没有它，单字查询（`记`、`屿`）在索引里查不到东西，只能逐篇全扫
//! （10k 库 4.4 s）；有了它，`"记"*` 是一条合法的 FTS5 前缀查询。
//!
//! **两侧的方向必须记牢：索引侧 ⊇ 查询侧。** 索引侧漏一个 bigram，多字
//! 查询就漏召回；查询侧多一个单字 token，`笔记` 就搜不到 `笔记库`——后者的
//! token 是 `笔记`/`识库`/`库`，里面没有 `记`。

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

/// 把正文切成极大的 CJK run，逐个交给回调。
///
/// 回调拿到的是 `(run 的字符切片, run 前面的 Word 字符, run 后面的 Word 字符)`。
/// 两侧的 `Option<char>` 只有「那个字符真的是 `Word`」时才是 `Some`——
/// 空格、标点、emoji 都不算，它们不产生跨脚本边界 bigram。
fn for_each_run(text: &str, mut f: impl FnMut(&[char], Option<char>, Option<char>)) {
    let chars: Vec<char> = text.chars().collect();
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

        let prev = start
            .checked_sub(1)
            .and_then(|j| chars.get(j))
            .copied()
            .filter(|c| is_word(*c));
        let next = chars.get(i).copied().filter(|c| is_word(*c));
        f(&chars[start..i], prev, next);
    }
}

/// **查询侧**的通道 B token：只有 bigram。
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
///
/// # 这里为什么**不**发单字 token
///
/// 单字 token 只有索引侧需要（见 [`index_tokens`]）。放进查询侧会直接漏召回：
/// `笔记` 的表达式会变成 `"笔记" AND "记"`，而 `笔记库` 的 token 是
/// `笔记`/`识库`/`库`——它含 `笔记` 却不含 `记`，于是搜不到。
///
/// 单字查询也不经过这里：[`crate::plan`] 直接产出 `"记"*`。
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_run(text, |run, prev, next| {
        for w in run.windows(2) {
            out.push(pair(w[0], w[1]));
        }
        if let Some(prev) = prev {
            out.push(pair(prev, run[0]));
        }
        if let Some(next) = next {
            out.push(pair(run[run.len() - 1], next));
        }
    });
    out
}

/// **索引侧**的通道 B token：查询侧的 bigram + 每个 run 末字符的单字 token。
///
/// 多出来的那一个 token 是**覆盖**用的，它保证：
///
/// > 正文里出现的每个 CJK 字，都至少有一个以它开头的 token。
///
/// 于是单字查询 `记` 能写成 FTS5 前缀表达式 `"记"*`，不必逐篇全扫。
/// 证明是一行：run 里位置 `0..n-2` 的字都是某个内部 bigram 的首字，
/// 位置 `n-1`（末字符）由这里补的那一个 token 兜住。长度 1 的 run 同理。
///
/// # 它为什么不会污染多字查询
///
/// 查询表达式里的每个分量都是 `"双字"`（[`tokens`] 从不发单字），而
/// `"知识"` 只匹配 token `知识` 本身。索引里多出来的 `识` 单字 token
/// 不在任何 AND 分量里，候选集因此逐条不变——`tests/search_total.rs` 的
/// 1000 / 1000 / 271 三条绝对数就是这条的判据。
///
/// # 代价
///
/// 每个 CJK run 多一个 token，所以增量是「**每个 run** 一个」，不是「每篇
/// 一个」——标点密的正文会切出很多短 run。10k 夹具（每篇约 11 个 run / 66 个
/// bigram）实测：`index.sqlite` **+15.1%**，建库时间 −0.5%（噪声内）。
/// 贵的主要不是 token 数量，而是新增的高文档频词（`的`、`上` 这些单字 term
/// 各有一条覆盖全库的 doclist）。
///
/// bm25 会看到略长的文档与这些新词，于是**候选排序**会变（最终顺序仍由
/// 定位器定）。
pub fn index_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_run(text, |run, prev, next| {
        for w in run.windows(2) {
            out.push(pair(w[0], w[1]));
        }
        // **索引侧独有**：run 的末字符。
        out.push(run[run.len() - 1].to_string());
        if let Some(prev) = prev {
            out.push(pair(prev, run[0]));
        }
        if let Some(next) = next {
            out.push(pair(run[run.len() - 1], next));
        }
    });
    out
}

/// 把 [`index_tokens`] 的结果拼成写进 FTS 列的字符串（单空格连接）。
///
/// 空列表返回空串。**空串不能拿去 MATCH**：`MATCH ''` 是语法错误
/// （`MATCH '""'` 才是空集），所以调用方必须先判空。
///
/// 写库只走这一条路：`ki-ipc` 的 `indexed_note` 与 `refresh` 共用它，
/// 两边算出来的 token 必须逐字节相同，否则同一篇笔记会因为「谁写的」
/// 而搜出不同的结果。
pub fn token_stream(text: &str) -> String {
    index_tokens(text).join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 查询侧：只有 bigram。
    fn qtoks(s: &str) -> Vec<&'static str> {
        leak(tokens(s))
    }

    /// 索引侧：bigram + 每个 run 末字符的覆盖 token。
    fn toks(s: &str) -> Vec<&'static str> {
        leak(index_tokens(s))
    }

    fn leak(tokens: Vec<String>) -> Vec<&'static str> {
        // 测试里借用返回的 String 不方便，这里返回一份泄漏的切片即可。
        // token 本身不含空白（bigram 两字、覆盖 token 一字），所以可以借
        // `split_whitespace` 再切回来。
        Box::leak(tokens.join(" ").into_boxed_str())
            .split_whitespace()
            .collect()
    }

    /// 方案里逐个钉死的例子。改规则必须先改这张表。
    ///
    /// **两列都要改**：查询侧与索引侧是两份不同的规则，只改一列就等着漏召回。
    #[test]
    fn segmentation_matches_the_spec_table() {
        // 查询侧：只有 bigram 与跨脚本边界 bigram。
        assert_eq!(qtoks("知识库"), ["知识", "识库"]);
        assert_eq!(qtoks("Rust的所有权"), ["的所", "所有", "有权", "t的"]);
        assert_eq!(qtoks("知识库Rust"), ["知识", "识库", "库R"]);
        assert_eq!(qtoks("的"), Vec::<&str>::new());
        assert_eq!(qtoks("🙂🙂"), Vec::<&str>::new());
        assert_eq!(qtoks("foo_bar"), Vec::<&str>::new());

        // 索引侧：每个 run 的末字符多一个单字 token。
        assert_eq!(toks("知识库"), ["知识", "识库", "库"]);
        assert_eq!(
            toks("Rust的所有权"),
            ["的所", "所有", "有权", "权", "t的"]
        );
        assert_eq!(toks("知识库Rust"), ["知识", "识库", "库", "库R"]);
        assert_eq!(toks("的"), ["的"]);
        assert_eq!(toks("🙂🙂"), Vec::<&str>::new());
        assert_eq!(toks("foo_bar"), Vec::<&str>::new());
    }

    /// 长度 1 的 run：查询侧一个 token 都没有，索引侧只有那一个覆盖 token。
    ///
    /// 这条是「单字查询能不能走索引」的全部依据：`tokens("的")` 是空的，
    /// 但 `index_tokens("的")` 有 `的`，于是 `"的"*` 查得到东西。
    #[test]
    fn single_cjk_run_yields_only_the_coverage_token() {
        assert!(tokens("的").is_empty());
        assert_eq!(toks("的"), ["的"]);

        // 相邻的 Word 仍然能给出边界 bigram，两者并存。
        assert_eq!(qtoks("A的"), ["A的"]);
        assert_eq!(toks("A的"), ["的", "A的"]);
        assert_eq!(qtoks("的A"), ["的A"]);
        assert_eq!(toks("的A"), ["的", "的A"]);

        // 空格把两侧都隔开了，没有边界 bigram 可言。
        assert!(tokens("A 的").is_empty());
        assert_eq!(toks("A 的"), ["的"]);
    }

    /// **本刀的核心不变量**：正文里出现的每个 CJK 字，索引里都有一个以它
    /// 开头的 token。
    ///
    /// 少了它，单字查询就会漏召回——不报错、不抛异常，只是「搜不到」。
    /// 用随机串而不是固定例子，是因为洞的成因有三类（行尾、空白、标点），
    /// 固定的几张表覆盖不全。
    #[test]
    fn every_cjk_char_has_a_token_starting_with_it() {
        use std::collections::HashSet;

        let mut rng = XorShift::new(0x1234_5678_9ABC_DEF0);
        let alphabet: Vec<char> =
            "知识库的设计实现原语抽象岛屿边界检索索引分词投影，。、 a1🙂".chars().collect();

        for _ in 0..300 {
            let len = 1 + rng.below(40);
            let text: String = (0..len).map(|_| alphabet[rng.below(alphabet.len())]).collect();
            let prefixes: HashSet<char> = index_tokens(&text)
                .iter()
                .filter_map(|t| t.chars().next())
                .collect();

            for c in text.chars().filter(|c| is_cjk(*c)) {
                assert!(
                    prefixes.contains(&c),
                    "漏召回：{text:?} 里的 {c} 没有任何以它开头的 token"
                );
            }
        }
    }

    /// 查询侧**不许**出现单字 token。
    ///
    /// 反例是具体的：查询 `笔记` 若拼成 `"笔记" AND "记"`，`笔记库` 就会
    /// 落选——它的 token 是 `笔记`/`识库`/`库`，含 `笔记` 却不含 `记`。
    #[test]
    fn query_tokens_never_include_coverage_tokens() {
        for text in ["笔记", "笔记库", "知识", "的", "哈哈", "知识库Rust"] {
            for token in tokens(text) {
                assert!(
                    token.chars().count() >= 2,
                    "{text:?} 的查询侧出现了单字 token {token:?}"
                );
            }
        }
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
        assert_eq!(qtoks("你好，世界"), ["你好", "世界"]);
        assert_eq!(toks("你好，世界"), ["你好", "好", "世界", "界"]);
    }

    /// 全角字母数字属于 `Word`（由通道 A 负责），不是 CJK。
    #[test]
    fn fullwidth_alphanumerics_are_word_chars() {
        assert!(!is_cjk('Ａ'));
        assert!(!is_cjk('１'));
        assert_eq!(qtoks("Ａ的"), ["Ａ的"]);
        assert_eq!(toks("Ａ的"), ["的", "Ａ的"]);
    }

    /// 重复必须保留：`哈哈` 在 FTS 里的词频要能被 bm25 看见。
    #[test]
    fn duplicates_are_preserved() {
        assert_eq!(qtoks("哈哈"), ["哈哈"]);
        assert_eq!(qtoks("哈哈哈"), ["哈哈", "哈哈"]);
        // 覆盖 token 也跟着重复一次，不影响 bigram 的词频。
        assert_eq!(toks("哈哈"), ["哈哈", "哈"]);
        assert_eq!(toks("哈哈哈"), ["哈哈", "哈哈", "哈"]);
    }

    /// 三种文字的 run 各自独立，跨脚本边界各发一次。
    #[test]
    fn mixed_scripts_get_boundary_bigrams_on_both_sides() {
        // 汉字 run 左右各贴一个 Word run
        assert_eq!(qtoks("a知识b"), ["知识", "a知", "识b"]);
        assert_eq!(toks("a知识b"), ["知识", "识", "a知", "识b"]);
    }

    /// 完备性：随机中文子串的 bigram 多重集必然被父串**索引侧**覆盖。
    ///
    /// 这是「bigram 不漏召回」的可执行形式，也是「索引侧 ⊇ 查询侧」的判据：
    /// 左边是查询会 AND 的全部 token，右边是真正写进 FTS 的那一列。用自带的
    /// xorshift 而不是引 `rand`，是为了让这个 crate 保持零依赖。
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
            let parent_tokens = index_tokens(&parent);

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
