//! 忽略规则：读用户配置、校验、合并成一张匹配表（ADR-014）。
//!
//! # 配置长什么样
//!
//! ```toml
//! [ignore]
//! extra  = ["我的笔记", "drafts"]   # 这些名字当目录/文件处理，不进树
//! negate = ["logseq"]              # 把这条内置规则停用（logseq 下的笔记回来了）
//! ```
//!
//! 只支持这一个段、这两个键。**不支持第二种格式**（不做 YAML / JSON 回退）：
//! 一种配置写坏时用户还有第二个地方可试，等于把「我的文件夹不见了」变成
//! 「我的哪个配置文件生效了」。
//!
//! # 为什么手写解析器而不是 `toml` crate
//!
//! 实测（`cargo build -p knowisle` 增量 + 二进制体积）：`toml = "1"` 因为
//! tauri 已经编过一份，增量在噪声内、体积 +13.5 KB；`toml = "0.8"` 会在
//! 已有的 1.x 之外**再装一份**，增量 +4.3 s、体积 +54 KB。也就是说，
//! **代价不是决定因素**——决定因素是三件别的事：
//!
//! 1. **错误文案是契约的一部分。** 判据要求「一句人话 + 路径 + 行号」，
//!    而 `toml` 给的是 `invalid type: string "dist", expected a sequence`。
//!    要把它翻译成人话，仍然得自己写一层语义校验——那已经是这个解析器的
//!    大半工作量了。
//! 2. **要拒绝的东西比要支持的多。** 日期、浮点、内联表、点号键、数组的数组，
//!    真正的 TOML 全都接受，而对本应用它们**全是写错**。用 crate 的话，
//!    我们得在它之后再加一层「这个值我们不认识」的判定。
//! 3. **依赖面。** `ki-vault` 只有 5 个运行时依赖，每一个都在 `Cargo.toml`
//!    里写了理由。为一个两键配置引入一个完整 TOML 实现，先例比代价更贵。
//!
//! 维护成本：语法子集是**冻结**的（两个键），新增键时才动它；每条错误都有
//! 用例钉住行号。估计每年不到半天。
//!
//! # 读配置只在扫描开头发生一次
//!
//! [`load`] 由 `scan_vault` 在**开头**调一次，结果编译成 [`Matcher`] 之后
//! 才进 `filter_entry`。绝不能在 `filter_entry` 里读文件——那是每个目录一次
//! `open`，10k 库上直接把扫描拖成秒级。

use std::path::{Path, PathBuf};

use ki_core::ignore::{
    is_builtin, is_negotiable, IgnoreConfigError, IgnoreRules, BUILTIN_DIRS, BUILTIN_FILES,
};
use ki_core::METADATA_DIR;

/// 配置文件名。与 [`METADATA_DIR`] 拼成 `<vault>/.knowisle/config.toml`。
///
/// # 为什么住在 `.knowisle` 里
///
/// 它是**用户输入**，而 `.knowisle` 是派生数据的家——这看起来矛盾，ADR-014
/// 专门回答：整库重建只删 `index.sqlite`（及 `-wal` / `-shm`），不删目录，
/// 所以配置不会被「重建」带走；而放在库根（`<vault>/knowisle.toml`）会让它
/// 出现在笔记树里（`.toml` 不是笔记扩展名，但用户会看见一个不属于自己的
/// 文件），并且与「这个目录里只有你的 Markdown」这条承诺冲突。
pub const CONFIG_FILE: &str = "config.toml";

/// 配置文件的绝对路径。
pub fn config_path(root: &Path) -> PathBuf {
    root.join(METADATA_DIR).join(CONFIG_FILE)
}

/// 读一次配置，返回**永远可用**的规则。
///
/// # 它不会失败
///
/// 文件不存在 → 内置默认（正常，不报警）；读不出来 / 语法错 / 语义错 →
/// 内置默认 + 一条 `config_error`。三种结果都让库打得开——这是 B-05 的硬约束，
/// 也是判据 5。
///
/// 返回值里的 `config_error.path` 是**绝对路径**：用户可能同时开着几个库，
/// 只说「config.toml 第 3 行」等于让他去猜是哪个。
pub fn load(root: &Path) -> IgnoreRules {
    let path = config_path(root);
    let shown = path.to_string_lossy().into_owned();

    let mut rules = match std::fs::read_to_string(&path) {
        Ok(text) => match parse(&text) {
            Ok(user) => IgnoreRules::with_user(user.extra, user.negate),
            Err(err) => IgnoreRules::with_error(IgnoreConfigError {
                path: shown.clone(),
                line: Some(err.line),
                message: err.message,
            }),
        },
        // 不存在是**正常状态**：绝大多数库没有这个文件，行为必须与改动前
        // 逐字节相同（判据 1）。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => IgnoreRules::builtin_only(),
        Err(err) => IgnoreRules::with_error(IgnoreConfigError {
            path: shown.clone(),
            // 整个文件都读不出来时行号没有意义。
            line: None,
            message: if err.kind() == std::io::ErrorKind::InvalidData {
                "配置文件不是 UTF-8：请用 UTF-8 保存（记事本的「UTF-8」即可）。".to_owned()
            } else {
                format!("配置文件读不出来：{err}")
            },
        }),
    };

    // 「去看过哪个文件」与「读到了什么」是两件事：文件不存在时规则是内置默认，
    // 但路径仍然要带出去——面板得告诉用户**该往哪儿写**。
    rules.config_path = Some(shown);
    rules
}

/// 解析并校验过的用户配置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserIgnore {
    pub extra: Vec<String>,
    pub negate: Vec<String>,
}

/// 解析 / 校验失败。`line` 从 1 开始。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfgError {
    pub line: u32,
    pub message: String,
}

impl CfgError {
    fn at(line: u32, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

/* ───────────────────────── 词法 ───────────────────────── */

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    OpenBracket,
    CloseBracket,
    Equals,
    Comma,
    /// 带引号的字符串（转义已解开）。
    Text(String),
    /// 裸标识符：段名、键名。值里出现它说明用户漏了引号。
    Word(String),
}

#[derive(Debug, Clone)]
struct Lexeme {
    token: Token,
    line: u32,
}

/// 把文本切成词。注释、空白、换行都在这里被吃掉——换行不进 token 流，
/// 因为「值能不能跨行」这件事由数组自己的语法决定，不需要额外的规则。
fn tokenize(text: &str) -> Result<Vec<Lexeme>, CfgError> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        let token = match c {
            '[' => {
                i += 1;
                Token::OpenBracket
            }
            ']' => {
                i += 1;
                Token::CloseBracket
            }
            '=' => {
                i += 1;
                Token::Equals
            }
            ',' => {
                i += 1;
                Token::Comma
            }
            '"' => {
                let start = line;
                i += 1;
                let mut value = String::new();
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return Err(CfgError::at(
                            start,
                            "字符串没有闭合：缺少结尾的 \"。",
                        ));
                    };
                    match ch {
                        '"' => {
                            i += 1;
                            break;
                        }
                        // TOML 的基本字符串不允许裸换行。允许的话，少一个引号
                        // 会把后面几行全吞进来，错误信息指向一个莫名其妙的位置。
                        '\n' => {
                            return Err(CfgError::at(
                                start,
                                "字符串没有闭合：缺少结尾的 \"（字符串不能跨行）。",
                            ))
                        }
                        '\\' => {
                            i += 1;
                            let Some(&escaped) = chars.get(i) else {
                                return Err(CfgError::at(start, "字符串没有闭合：缺少结尾的 \"。"));
                            };
                            match escaped {
                                '\\' => value.push('\\'),
                                '"' => value.push('"'),
                                'n' => value.push('\n'),
                                't' => value.push('\t'),
                                'r' => value.push('\r'),
                                other => {
                                    return Err(CfgError::at(
                                        line,
                                        format!(
                                            "不认识的转义 \\{other}：只支持 \\\\、\\\"、\\n、\\t、\\r。"
                                        ),
                                    ))
                                }
                            }
                            i += 1;
                        }
                        _ => {
                            value.push(ch);
                            i += 1;
                        }
                    }
                }
                Token::Text(value)
            }
            // 单引号是「字面字符串」：里面一个转义都没有。Windows 路径写错了
            // 的时候它比基本字符串更宽容，值得支持（虽然路径本身会被拒绝）。
            '\'' => {
                let start = line;
                i += 1;
                let mut value = String::new();
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return Err(CfgError::at(start, "字符串没有闭合：缺少结尾的 '。"));
                    };
                    if ch == '\'' {
                        i += 1;
                        break;
                    }
                    if ch == '\n' {
                        return Err(CfgError::at(
                            start,
                            "字符串没有闭合：缺少结尾的 '（字符串不能跨行）。",
                        ));
                    }
                    value.push(ch);
                    i += 1;
                }
                Token::Text(value)
            }
            c if is_word_char(c) => {
                let mut word = String::new();
                while i < chars.len() && is_word_char(chars[i]) {
                    word.push(chars[i]);
                    i += 1;
                }
                Token::Word(word)
            }
            other => {
                return Err(CfgError::at(
                    line,
                    format!("不认识的字符 `{other}`：这个文件只支持 [ignore] 段与两个字符串数组。"),
                ))
            }
        };

        out.push(Lexeme { token, line });
    }

    Ok(out)
}

/// 裸标识符允许的字符。
///
/// 含 `.` 与 `-`：`[ignore]`、`extra`、以及将来可能出现的 `extra-2` 都不该
/// 被词法挡住。**不含 `/` 与 `\`**：那是路径，在值里必须带引号，而带引号之后
/// 会被语义校验拦下来并给出「这是路径不是名字」。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/* ───────────────────────── 语法 ───────────────────────── */

/// 解析一份配置。
///
/// 语法就是「段 + 键值对 + 字符串数组」，逗号可选（漏写逗号与多写尾逗号都
/// 不报错——它们不改变语义）。除此之外的一切都是错误，且**都带行号**。
fn parse(text: &str) -> Result<UserIgnore, CfgError> {
    let lexemes = tokenize(text)?;
    let mut i = 0usize;
    let mut in_ignore = false;
    let mut seen_section = false;
    let mut extra: Option<Vec<(String, u32)>> = None;
    let mut negate: Option<Vec<(String, u32)>> = None;

    while i < lexemes.len() {
        let lexeme = &lexemes[i];
        match &lexeme.token {
            Token::OpenBracket => {
                let line = lexeme.line;
                i += 1;
                let name = match lexemes.get(i).map(|l| &l.token) {
                    Some(Token::Word(word)) => {
                        let word = word.clone();
                        i += 1;
                        word
                    }
                    _ => return Err(CfgError::at(line, "段名缺失：这里应该写 [ignore]。")),
                };
                match lexemes.get(i).map(|l| &l.token) {
                    Some(Token::CloseBracket) => i += 1,
                    _ => {
                        return Err(CfgError::at(
                            line,
                            format!("段名没有闭合：应该是 [{name}]。"),
                        ))
                    }
                }
                if !name.eq_ignore_ascii_case("ignore") {
                    return Err(CfgError::at(
                        line,
                        format!("不认识的段 [{name}]：这个文件只支持 [ignore]。"),
                    ));
                }
                if seen_section {
                    return Err(CfgError::at(line, "[ignore] 段写了两次。"));
                }
                seen_section = true;
                in_ignore = true;
            }
            Token::Word(key) => {
                let key = key.clone();
                let line = lexeme.line;
                i += 1;
                // 段先判：写在段外面的键，报「缺段」比报「值类型不对」有用得多。
                if !in_ignore {
                    return Err(CfgError::at(
                        line,
                        format!("键 {key} 必须写在 [ignore] 段里。"),
                    ));
                }
                match lexemes.get(i).map(|l| &l.token) {
                    Some(Token::Equals) => i += 1,
                    _ => return Err(CfgError::at(line, format!("{key} 后面缺少 =。"))),
                }
                let values = parse_array(&lexemes, &mut i, &key, line)?;
                let slot = match key.as_str() {
                    "extra" => &mut extra,
                    "negate" => &mut negate,
                    other => {
                        return Err(CfgError::at(
                            line,
                            format!("不认识的键 {other}：这个文件只支持 extra 与 negate。"),
                        ))
                    }
                };
                if slot.is_some() {
                    return Err(CfgError::at(line, format!("{key} 写了两次。")));
                }
                *slot = Some(values);
            }
            _ => {
                return Err(CfgError::at(
                    lexeme.line,
                    "这里应该是一个段（[ignore]）或一个键（extra = [...]）。",
                ))
            }
        }
    }

    validate(extra.unwrap_or_default(), negate.unwrap_or_default())
}

/// 解析一个字符串数组。逗号可有可无，尾逗号合法。
fn parse_array(
    lexemes: &[Lexeme],
    i: &mut usize,
    key: &str,
    key_line: u32,
) -> Result<Vec<(String, u32)>, CfgError> {
    let Some(open) = lexemes.get(*i) else {
        return Err(CfgError::at(key_line, format!("{key} 后面缺少值。")));
    };

    let open_line = open.line;
    match &open.token {
        Token::OpenBracket => *i += 1,
        // 判据 5 的那一条：`extra = "dist"` 必须给一句人话，而不是「类型错误」。
        Token::Text(_) => {
            return Err(CfgError::at(
                open.line,
                format!(
                    "{key} 必须是字符串数组（{key} = [\"dist\"]），这里是一个字符串。"
                ),
            ))
        }
        _ => {
            return Err(CfgError::at(
                open.line,
                format!("{key} 必须是字符串数组（{key} = [\"dist\"]）。"),
            ))
        }
    }

    let mut out = Vec::new();
    loop {
        let Some(item) = lexemes.get(*i) else {
            return Err(CfgError::at(open_line, format!("{key} 的数组没有闭合：缺少 ]。")));
        };
        match &item.token {
            Token::CloseBracket => {
                *i += 1;
                return Ok(out);
            }
            Token::Comma => *i += 1,
            Token::Text(value) => {
                out.push((value.clone(), item.line));
                *i += 1;
            }
            Token::Word(word) => {
                return Err(CfgError::at(
                    item.line,
                    format!("{key} 的数组里只能放字符串：{word} 少了引号。"),
                ))
            }
            _ => {
                return Err(CfgError::at(
                    item.line,
                    format!("{key} 的数组里只能放字符串。"),
                ))
            }
        }
    }
}

/* ───────────────────────── 语义 ───────────────────────── */

/// 校验规则本身。任何一条不合法都**整体退回内置默认**——半个配置比没有配置
/// 更难解释：用户会以为生效的那一半就是全部。
fn validate(
    extra: Vec<(String, u32)>,
    negate: Vec<(String, u32)>,
) -> Result<UserIgnore, CfgError> {
    let extra = check_names(extra, "extra")?;
    let negate = check_names(negate, "negate")?;

    for (name, line) in &negate {
        // `.git` / `.knowisle` 不是「静默忽略你的 negate」，而是一条**可读的
        // 拒绝理由**：用户必须知道它没生效，以及为什么。
        if !is_negotiable(name) {
            return Err(CfgError::at(
                *line,
                format!(
                    "negate 不能停用 \"{name}\"：.git 与 .knowisle 永远被忽略\
                     （前者是版本库，遍历它会拖垮扫描；后者是本应用自己的索引、\
                     回收站与配置）。"
                ),
            ));
        }
        if !is_builtin(name) {
            // 拼错的 negate 是最坏的一种「看起来生效了」：树没变，而用户以为
            // 自己已经改好了。宁可报错。
            return Err(CfgError::at(
                *line,
                format!(
                    "negate 里的 \"{name}\" 不在内置名单里，没有东西可停用\
                     （只能停用下面列出的内置规则）。"
                ),
            ));
        }
    }

    for (name, line) in &extra {
        if negate
            .iter()
            .any(|(other, _)| other.eq_ignore_ascii_case(name))
        {
            return Err(CfgError::at(
                *line,
                format!("\"{name}\" 同时出现在 extra 与 negate 里：不知道该忽略还是该显示。"),
            ));
        }
    }

    Ok(UserIgnore {
        extra: dedupe(extra),
        negate: dedupe(negate),
    })
}

/// 逐个名字的检查：非空、不是路径、不是通配符、不含控制字符。
fn check_names(entries: Vec<(String, u32)>, key: &str) -> Result<Vec<(String, u32)>, CfgError> {
    for (name, line) in &entries {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(CfgError::at(*line, format!("{key} 里不能有空名字。")));
        }
        if trimmed != name {
            return Err(CfgError::at(
                *line,
                format!("{key} 里的 \"{name}\" 首尾有空白：名字按字面匹配，空白会让它永远不命中。"),
            ));
        }
        if name.contains('/') || name.contains('\\') {
            return Err(CfgError::at(
                *line,
                format!(
                    "\"{name}\" 是路径，不是名字：忽略规则只按目录名 / 文件名匹配\
                     （不分大小写），不看路径。写 \"dist\" 会忽略**所有**叫 dist 的目录。"
                ),
            ));
        }
        if name.contains('*') || name.contains('?') {
            return Err(CfgError::at(
                *line,
                format!("\"{name}\" 里有通配符：这里写的是名字，不是模式。"),
            ));
        }
        if name.chars().any(|c| c.is_control()) {
            return Err(CfgError::at(
                *line,
                format!("{key} 里的名字含控制字符，配置文件可能被别的工具改坏了。"),
            ));
        }
    }
    Ok(entries)
}

/// 去重，保留第一次出现的顺序。
///
/// 重复写同一个名字是**无害**的（用户从别处粘过来时很常见），不值得报错。
fn dedupe(entries: Vec<(String, u32)>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for (name, _) in entries {
        if !out.iter().any(|kept| kept.eq_ignore_ascii_case(&name)) {
            out.push(name);
        }
    }
    out
}

/* ───────────────────────── 匹配表 ───────────────────────── */

/// 一次扫描用的匹配表。
///
/// # 为什么要在扫描开头编译一次
///
/// `filter_entry` 会对**每一个目录项**调用一次。在里面做「内置 − negate +
/// extra」的集合运算，等于把配置解析的开销乘上条目数。这里在开头算好两张
/// `Vec<String>`，之后每个条目只是几次 `eq_ignore_ascii_case`。
///
/// # 为什么大小写不敏感
///
/// Windows 的文件系统不区分大小写，Linux 区分。配置里的 `Dist` 在 Windows 上
/// 必须命中 `dist`——所以配置侧**不许**写 `==`，必须与扫描器走同一条
/// `eq_ignore_ascii_case`。反过来的代价是 Linux 上 `Dist` 也会命中 `dist`：
/// 这是刻意的，一份配置要在三平台行为一致。
pub(crate) struct Matcher {
    dirs: Vec<String>,
    files: Vec<String>,
}

impl Matcher {
    pub(crate) fn new(rules: &IgnoreRules) -> Self {
        Self {
            dirs: effective(BUILTIN_DIRS, rules),
            files: effective(BUILTIN_FILES, rules),
        }
    }

    pub(crate) fn ignores_dir(&self, name: &str) -> bool {
        self.dirs.iter().any(|known| known.eq_ignore_ascii_case(name))
    }

    pub(crate) fn ignores_file(&self, name: &str) -> bool {
        self.files.iter().any(|known| known.eq_ignore_ascii_case(name))
    }
}

/// 内置名单 − negate + extra。
///
/// `is_negotiable` 这一道是**纵深防御**：`load` 已经拒绝了 `.git` / `.knowisle`，
/// 但 `IgnoreRules` 也能被测试或将来的 CLI 手工构造。少一道，一个手写的
/// `negate` 就能让扫描器走进 `.git` 的 objects。
fn effective(builtin: &[&str], rules: &IgnoreRules) -> Vec<String> {
    let mut out = Vec::with_capacity(builtin.len() + rules.extra.len());
    for name in builtin {
        if is_negotiable(name)
            && rules
                .negate
                .iter()
                .any(|negated| negated.eq_ignore_ascii_case(name))
        {
            continue;
        }
        out.push((*name).to_owned());
    }
    out.extend(rules.extra.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> CfgError {
        parse(text).expect_err("这份配置应当被拒绝")
    }

    fn ok(text: &str) -> UserIgnore {
        parse(text).expect("这份配置应当通过")
    }

    #[test]
    fn parses_the_documented_shape() {
        let user = ok("[ignore]\nextra = [\"我的笔记\", \"drafts\"]\nnegate = [\"logseq\"]\n");
        assert_eq!(user.extra, vec!["我的笔记", "drafts"]);
        assert_eq!(user.negate, vec!["logseq"]);
    }

    /// 多行数组 + 注释 + 尾逗号：这是用户真正会写的样子。
    #[test]
    fn parses_multiline_arrays_comments_and_trailing_comma() {
        let user = ok(
            "# 我的忽略规则\n\
             [ignore]\n\
             extra = [\n\
             \x20 \"我的笔记\",   # 这个目录里是草稿\n\
             \x20 \"dist\",\n\
             ]\n",
        );
        assert_eq!(user.extra, vec!["我的笔记", "dist"]);
        assert!(user.negate.is_empty());
    }

    #[test]
    fn empty_file_means_no_user_rules() {
        let user = ok("");
        assert!(user.extra.is_empty() && user.negate.is_empty());
    }

    /// 判据 5：`extra = "dist"` 必须给一句人话，且指到那一行。
    #[test]
    fn string_instead_of_array_is_a_readable_error_with_a_line() {
        let e = err("[ignore]\nextra = \"dist\"\n");
        assert_eq!(e.line, 2);
        assert!(
            e.message.contains("必须是字符串数组") && e.message.contains("字符串"),
            "文案要说清「这里是一个字符串」：{}",
            e.message
        );
    }

    /// 判据 3 的另一半：`.git` 不可 negate，且要说清为什么。
    #[test]
    fn git_and_metadata_dir_can_never_be_negated() {
        for name in [".git", ".knowisle", ".GIT"] {
            let e = err(&format!("[ignore]\nnegate = [\"{name}\"]\n"));
            assert_eq!(e.line, 2);
            assert!(
                e.message.contains("永远被忽略") && e.message.contains(".git"),
                "拒绝理由必须点名并解释：{}",
                e.message
            );
        }
    }

    #[test]
    fn negating_an_unknown_name_is_an_error() {
        let e = err("[ignore]\nnegate = [\"logseqg\"]\n");
        assert!(e.message.contains("不在内置名单里"), "{}", e.message);
    }

    /// 路径不是名字。这是 B-05 最容易被误解的一处，必须给得出「为什么」。
    ///
    /// 反斜杠那一例用**单引号**字面字符串：`\d` 在基本字符串里是非法转义，
    /// 用双引号会先撞上词法错误，测不到这条语义。
    #[test]
    fn paths_are_rejected_with_an_explanation() {
        for config in [
            "[ignore]\nextra = [\"my-notes/dist\"]\n",
            "[ignore]\nextra = ['my-notes\\dist']\n",
        ] {
            let e = err(config);
            assert!(
                e.message.contains("是路径，不是名字"),
                "{config} → {}",
                e.message
            );
        }
    }

    #[test]
    fn wildcards_are_rejected() {
        let e = err("[ignore]\nextra = [\"*.tmp\"]\n");
        assert!(e.message.contains("通配符"), "{}", e.message);
    }

    #[test]
    fn a_name_cannot_be_both_extra_and_negate() {
        let e = err("[ignore]\nextra = [\"logseq\"]\nnegate = [\"logseq\"]\n");
        assert!(e.message.contains("同时出现"), "{}", e.message);
    }

    #[test]
    fn unknown_key_and_section_are_rejected() {
        assert!(err("[ignore]\nextar = [\"a\"]\n").message.contains("不认识的键"));
        assert!(err("[other]\nextra = [\"a\"]\n").message.contains("不认识的段"));
        assert!(err("extra = [\"a\"]\n").message.contains("必须写在 [ignore] 段里"));
    }

    #[test]
    fn syntax_errors_point_at_the_right_line() {
        assert_eq!(err("[ignore]\nextra = [\"a\"\n").line, 2, "数组没闭合");
        assert_eq!(err("[ignore]\nextra = [\"a]\n").line, 2, "字符串没闭合");
        assert_eq!(err("[ignore]\nextra [\"a\"]\n").line, 2, "缺等号");
    }

    #[test]
    fn duplicates_are_collapsed_case_insensitively() {
        let user = ok("[ignore]\nextra = [\"dist\", \"DIST\", \"我的笔记\"]\n");
        assert_eq!(user.extra, vec!["dist", "我的笔记"]);
    }

    /// 匹配表：内置 − negate + extra，且 `.git` / `.knowisle` 挡得住手写的 negate。
    #[test]
    fn matcher_applies_negate_and_never_unignores_the_fixed_ones() {
        let rules = IgnoreRules::with_user(vec!["我的笔记".to_owned()], vec!["logseq".to_owned()]);
        let matcher = Matcher::new(&rules);
        assert!(!matcher.ignores_dir("logseq"), "negate 之后应能看见");
        assert!(
            !matcher.ignores_dir("logseq-notes"),
            "按名字整段匹配，不是前缀"
        );
        assert!(matcher.ignores_dir("我的笔记"));
        assert!(matcher.ignores_dir("dist"), "没被 negate 的内置仍然生效");

        // 绕过 `load` 的手写规则：纵深防御必须挡住。
        let rogue = IgnoreRules::with_user(Vec::new(), vec![".git".to_owned()]);
        let matcher = Matcher::new(&rogue);
        assert!(matcher.ignores_dir(".git"), ".git 永远不可 negate");
        assert!(matcher.ignores_dir(".knowisle"), ".knowisle 永远不可 negate");
    }
}
