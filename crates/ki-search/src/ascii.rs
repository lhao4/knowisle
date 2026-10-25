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
//! 词首前缀把损失压到最小（`ru`、`rus`、`rust` 都能中），
//! 而 `us` 这类查询本来就更像误触。哪天需要补回来，正确做法是加 FTS5
//! 的 `trigram` 第三通道，而不是把这个函数改回子串。

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
}
