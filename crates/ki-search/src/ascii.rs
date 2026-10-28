//! 通道 A 的 ASCII 投影。
//!
//! # 为什么必须投影，而不是把原文直接塞给 FTS5
//!
//! `unicode61` 不区分脚本：`Rust的所有权` 在它眼里是**一个** token。
//! 于是查 `rust`（精确）命中不了；查 `rust*`（前缀）能命中——但只因为
//! ASCII 恰好在汉字**前面**。`本地优先Rust` 这种「ASCII 在汉字后面」的写法，
//! 连前缀都不行。
//!
//! 把非 ASCII 全部抹成空格之后，ASCII 词各自成 token，行为就完全可预测了：
//! 查什么词、命中哪个词，与它在中文里的位置无关。
//!
//! # 已知收窄（写进 ADR-007 与 BACKLOG）
//!
//! 投影之后只剩整词与前缀匹配，**词中子串不保证**：
//! `us` 搜不到 `Rust`。旧引擎（内存子串匹配）是能搜到的，所以这是一处
//! 有意的能力收窄——换来的是索引体积与内存（旧引擎常驻约两倍正文）。
//!
//! # 词中子串：`trigram` 修不了，词内 bigram 能
//!
//! 这里曾经写着「正确做法是加 FTS5 的 `trigram` 第三通道」。那句话是错的，
//! 两个理由都是实测的：
//!
//! 1. **trigram 有 3 字符下限。** SQLite 文档（FTS5 4.3.4）原文：
//!    "Substrings consisting of fewer than 3 unicode characters do not match
//!    any rows when used with a full-text query." 本机实测（rusqlite 0.32
//!    bundled，SQLite 3.46.0）：正文 `rust`，`MATCH 'us'` / `'ow'` / `'ru'` /
//!    `'st'` 全部 0 行，`MATCH 'rus'` / `'rust'` 各 1 行。而当初那个例子
//!    恰好是 2 字符的 `us`——trigram 通道**修不了它**。
//! 2. **trigram 更贵。** 同一份 10k 真实语料夹具上，`index.sqlite`
//!    4.69 MiB → 6.30 MiB（**+34.3%**），超过本刀的 30% 预算；词内 bigram
//!    是 5.66 MiB（**+20.7%**）。
//!
//! 所以通道 A 补的是**词内 bigram**（[`bigrams`]）：`rust` 索引成
//! `ru us st`，查询 `us` 直接命中 token `us`。见 [`bigrams`] 的召回证明。
//! `trigram` 这条路的实测数字留在 ADR-007，别再走一遍。

/// 把正文投影成「只含小写 ASCII 字母数字与单空格」的串。
///
/// 规则：
/// - ASCII 字母 → 小写；ASCII 数字原样；
/// - 其余一切（汉字、全角字母、下划线、连字符、标点、emoji）→ 空格；
/// - 连续空白压缩成一个空格，去掉首尾。
///
/// 输出里**不可能**出现 `"` 或 FTS5 的运算符字符，因此拿它拼查询表达式
/// 不需要额外转义（查询侧仍然会 quote，那是防御性的第二道）。
///
/// 例：`Rust的所有权` → `rust`；`知识库` → 空串。
pub fn project(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;

    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c.to_ascii_lowercase());
        } else {
            // 非 ASCII 的字母数字也走这里：它们属于通道 B 或干脆不该被索引，
            // 在通道 A 里出现只会让 token 变成混合脚本，重新引入上面那个问题。
            pending_space = true;
        }
    }

    out
}

/// 把**投影后的正文**展开成词内 bigram：`ru us st`（`rust` 的三个相邻字符对）。
///
/// # 召回证明（通道 A 的子串语义靠这一条成立）
///
/// 设查询 `Q` 是长度 ≥2 的纯 ASCII 字母数字串，且 `Q` 在正文里作为**连续
/// 子串**出现。那么：
///
/// 1. `Q` 的每个字符都是 ASCII 字母数字，所以它整段落在正文的某个**极大
///    字母数字 run**（也就是 `project` 意义上的一个词 `W`）里——中间只要有
///    一个非字母数字字符，`Q` 就断成两截，与「连续」矛盾；
/// 2. `Q ⊆ W` 连续 ⟹ `Q` 的每个相邻字符对都是 `W` 的相邻字符对；
/// 3. [`bigrams`] 把 `W` 的**全部**相邻字符对写进 `body_bigram`，于是
///    `Q` 的每个 bigram 都是该篇的 token；
/// 4. 查询表达式是这些 bigram 的 AND，所以该篇必进候选。
///
/// 反过来**不**成立（|Q| ≥ 3 时）：`ab` 与 `bc` 可能来自两个不同的词，
/// 于是候选里会有不含 `Q` 的篇。那不要紧——索引只负责「不漏」，精度由
/// `NoteLocator` 逐行字面判定。
///
/// # 为什么是 bigram，不是 trigram
///
/// trigram 有 3 字符下限，`us` 这类 2 字符查询它一条都匹配不到（实测见
/// 文件头）。bigram 没有下限，且索引更小。代价是 |Q| ≥ 3 时多一层假阳性，
/// 由定位器过滤。
///
/// # 输入必须是 [`project`] 的输出
///
/// 这个函数**只**产出小写 ASCII 字母数字对：非字母数字字符既不入 token，
/// 也不与相邻字符配对（它把「前一个字符」清空）。于是即使有人把原文直接
/// 递进来，也不会产生中文 bigram 或跨脚本 bigram——「通道 A' 只装 ASCII
/// 投影」这条约束在函数里就成立，不靠调用方自觉。
///
/// 保留重复：`rust` 在一篇里出现 100 次就写 300 个 token。bm25 需要词频，
/// 而去重只省 0.5% 的体积（10k 夹具实测 5.66 → 5.63 MiB），不值得为它
/// 在写库路径上多一个 `HashSet`。
pub fn bigrams(projected: &str) -> String {
    let mut out = String::with_capacity(projected.len() * 2);
    let mut prev: Option<char> = None;

    for c in projected.chars() {
        if !c.is_ascii_alphanumeric() {
            prev = None;
            continue;
        }
        let c = c.to_ascii_lowercase();
        if let Some(p) = prev {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push(p);
            out.push(c);
        }
        prev = Some(c);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_words_survive_and_are_lowercased() {
        assert_eq!(project("The Rust Book"), "the rust book");
        assert_eq!(project("snake_case"), "snake case");
        assert_eq!(project("kebab-case"), "kebab case");
        assert_eq!(project("v1.2.3"), "v1 2 3");
    }

    /// 汉字在投影后完全消失，不会把相邻的 ASCII 粘成一个混合 token。
    #[test]
    fn cjk_is_erased_and_does_not_glue_ascii() {
        assert_eq!(project("Rust的所有权"), "rust");
        assert_eq!(project("本地优先Rust"), "rust");
        assert_eq!(project("知识库"), "");
        assert_eq!(project("中文rust中文"), "rust");
    }

    /// 全角字母数字不是 ASCII，投影后消失——它们由通道 B 当 `Word` 处理。
    #[test]
    fn fullwidth_alphanumerics_are_dropped() {
        assert_eq!(project("ＡＢＣ"), "");
        assert_eq!(project("１２３"), "");
    }

    /// 输出里不许出现引号或空白以外的任何非字母数字字符：
    /// 这是「投影结果可以直接拼进 MATCH」的前提。
    #[test]
    fn output_is_only_lowercase_alnum_and_single_spaces() {
        for input in [
            "a\"b",
            "a OR b",
            "NEAR(a b)",
            "-x",
            "a*b",
            "a\tb\nc",
            "😀x😀",
            "\u{3000}x\u{3000}",
        ] {
            let out = project(input);
            assert!(
                out.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' '),
                "{input:?} → {out:?} 含非法字符"
            );
            assert!(!out.starts_with(' ') && !out.ends_with(' '), "{out:?}");
            assert!(!out.contains("  "), "{out:?}");
        }
    }

    /// 词内 bigram：每个相邻字符对一个，顺序按词内从左到右。
    #[test]
    fn bigrams_are_every_adjacent_pair() {
        assert_eq!(bigrams(&project("Rust")), "ru us st");
        assert_eq!(bigrams(&project("us")), "us");
        assert_eq!(bigrams(&project("a")), "");
        assert_eq!(
            bigrams(&project("rust ownership")),
            "ru us st ow wn ne er rs sh hi ip"
        );
    }

    /// 跨词不配对：`a b` 不能产出 `ab`，否则「两个短词」会命中「一个长词」。
    #[test]
    fn bigrams_never_cross_a_word_boundary() {
        assert_eq!(bigrams("a b"), "");
        assert_eq!(bigrams("ab cd"), "ab cd");
        // 投影把 `_` 变成空格，所以 `snake_case` 的两个词各自配对。
        assert_eq!(bigrams(&project("snake_case")), "sn na ak ke ca as se");
    }

    /// **通道 A' 只装 ASCII 投影**：即使有人把原文直接递进来，也不许产出
    /// 中文 bigram 或跨脚本 bigram。这条不是靠调用方自觉，是函数性质。
    #[test]
    fn bigrams_are_ascii_only_even_on_raw_text() {
        assert_eq!(bigrams("中文"), "");
        assert_eq!(bigrams("ab中cd"), "ab cd", "汉字必须同时断开两侧");
        assert_eq!(bigrams("Rust的所有权"), "ru us st");
        assert_eq!(bigrams("本地优先Rust"), "ru us st");
        assert_eq!(bigrams("知识库rust知识库"), "ru us st");
        for input in ["ab中cd", "a\"b", "a OR b", "-x", "😀ab😀"] {
            let out = bigrams(input);
            assert!(
                out.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' '),
                "{input:?} → {out:?} 含非法字符"
            );
            assert!(!out.starts_with(' ') && !out.ends_with(' '), "{out:?}");
            assert!(!out.contains("  "), "{out:?}");
        }
    }

    /// 重复保留：`rust rust` 是 6 个 token，不是 3 个——bm25 要用词频。
    #[test]
    fn bigrams_keep_duplicates() {
        assert_eq!(bigrams("ru ru"), "ru ru");
        assert_eq!(bigrams(&project("rust rust")), "ru us st ru us st");
    }
}
