//! 文件名校验与规范化。
//!
//! 这是**唯一**的文件名闸门。前端只做即时反馈（提前说一句"这里有斜杠"），
//! 真正的判定在这里——将来 `isle` CLI 建笔记走的也是这条路，规则不能写在
//! Tauri 命令层，否则第二个入口就得抄一遍，而抄的那份迟早会漂移。
//!
//! # 为什么校验这么啰嗦
//!
//! 一个合法的笔记名要同时过**文件系统**与**扫描器**两套规则，而两者的失败
//! 方式完全不同：
//!
//! | 层面 | 规则 | 违反后的表现 |
//! |---|---|---|
//! | 文件系统 | Windows 保留名、`< > : " / \ | ? *`、尾部空格与点、255 字节 | 落盘失败，或名字被系统静默改写 |
//! | 扫描器 | 只认 `.md` / `.markdown` | 文件建出来了，树里永远看不见 |
//!
//! 第二类更坏：它不报错。用户会以为"新建失败了"，然后再建一次，于是磁盘上
//! 堆起一串看不见的文件。所以全角 `.ｍｄ` 这种"看起来有后缀"的输入必须
//! **显式拒绝并说明原因**，不能静默补一个 `.md` 了事。
//!
//! # 为什么连 Linux 上合法的字符也拒
//!
//! 库是要跨平台同步的（M4 的 Git）。一篇在 Linux 上叫 `a:b.md` 的笔记，
//! 到 Windows 上根本落不了盘；等到同步那天再发现，代价是"这条历史无法 checkout"。
//! 在入口处拒绝，代价只是一句提示。

use std::path::Path;

use ki_core::KiError;

/// 纳入笔记树的扩展名。**必须与 `scan::NOTE_EXTENSIONS` 一致**——
/// 这里决定"能不能建"，那里决定"看不看得见"，两份名单不一致就会出现
/// "建得出来但看不见"的文件。
const NOTE_EXTENSIONS: &[&str] = &["md", "markdown"];

/// 单段文件名的字节上限。
///
/// 255 是 ext4 / APFS / NTFS 的共同上限（NTFS 是 255 个 UTF-16 码元，
/// 对 BMP 内的字符与字节数等价；emoji 会占两倍，但那种名字本来就少见）。
/// 按字节算比按字符算保守，宁可早拒绝也不要让 `create_new` 抛一个裸 OS 错误。
const MAX_NAME_BYTES: usize = 255;

/// Windows 设备名。带不带扩展名都保留（`CON.md` 同样是设备）。
const RESERVED_STEMS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 文件系统层面非法的字符。`/` 与 `\` 在这里不是"分隔符"而是"非法字符"——
/// 本函数只接受**单个文件名**，带分隔符的输入是想移动文件，那不属于改名。
const ILLEGAL_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// 全角等价字符。它们与 ASCII 结构字符长得几乎一样，但文件系统不认。
///
/// 只收"会被误认成结构字符"的那几个。全角逗号 `，`、全角括号不在名单里：
/// 中文笔记名里出现它们是正常的，拒绝只会误伤。
const FULLWIDTH_LOOKALIKES: &[(char, &str)] = &[
    ('．', "."),
    ('。', "."),
    ('／', "/"),
    ('＼', "\\"),
    ('：', ":"),
    ('＊', "*"),
    ('？', "?"),
    ('＂', "\""),
    ('＜', "<"),
    ('＞', ">"),
    ('｜', "|"),
];

/// 构造一个"名字不合法"错误。`reason` 是给用户看的一句话，必须能指导下一步动作。
fn invalid(name: &str, reason: impl Into<String>) -> KiError {
    KiError::InvalidName {
        name: name.to_owned(),
        reason: reason.into(),
    }
}

/// 扩展名是否是扫描器认的那两个（ASCII 大小写不敏感）。
///
/// 与 `scan::is_note` 同一条规则：`Path::extension()` 而不是字符串切分，
/// 因为 `.md` 这种"只有扩展名"的名字在 `Path` 眼里**没有扩展名**——
/// 而扫描器正是按 `Path` 判的，这里必须跟它一致。
fn has_note_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            NOTE_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(ext))
        })
}

/// 校验并规范化一个笔记文件名，返回**最终会落到磁盘上的名字**。
///
/// 规范化只有一条：没有 `.md` / `.markdown` 后缀就补 `.md`。
/// 补后缀而不是报错，是因为"新建一篇叫 `周会` 的笔记"是最常见的输入；
/// 而已经带了后缀就不再重复补，否则会得到 `周会.md.md`。
///
/// # 拒绝清单
///
/// 空名、首尾空白、控制字符、`< > : " / \ | ? *`、尾部点、`CON` 系列保留名、
/// 超过 255 字节、全角结构字符、以及"规范化之后扫描器仍然看不见"的名字。
pub fn normalize_note_name(raw: &str) -> Result<String, KiError> {
    if raw.trim().is_empty() {
        return Err(invalid(raw, "名字不能为空"));
    }
    // 不 trim 之后继续用：静默去掉尾部空格会让"用户输入的"和"磁盘上的"
    // 不是同一个名字，下次再改名时对不上。宁可拒绝。
    if raw.trim() != raw {
        return Err(invalid(
            raw,
            "名字首尾不能有空白（Windows 会静默去掉尾部的空格与点）",
        ));
    }

    for ch in raw.chars() {
        if ch.is_control() {
            return Err(invalid(raw, "名字里不能有控制字符（换行、制表符等）"));
        }
        if ILLEGAL_CHARS.contains(&ch) {
            return Err(invalid(
                raw,
                format!("名字里不能有 {ch}（< > : \" / \\ | ? * 在 Windows 上非法，/ 与 \\ 还会被当成目录分隔符）"),
            ));
        }
        if let Some((_, ascii)) = FULLWIDTH_LOOKALIKES.iter().find(|(wide, _)| *wide == ch) {
            return Err(invalid(
                raw,
                format!("全角「{ch}」看起来像「{ascii}」，但文件系统与扫描器都不认；请用半角"),
            ));
        }
    }

    if raw.ends_with('.') {
        return Err(invalid(raw, "名字不能以点结尾（Windows 会静默去掉它）"));
    }
    // 以点开头的名字在资源管理器 / Finder 里是隐藏文件，而我们的树会把它
    // 照常显示出来——两边对"这个文件存不存在"的答案不一致。而且 `.md` 这种
    // 输入补完后缀会变成 `.md.md`，用户要的显然不是这个。一律拒绝。
    if raw.starts_with('.') {
        return Err(invalid(raw, "名字不能以点开头（会被当成隐藏文件）"));
    }

    // 保留名只看**第一个点之前**的部分：`CON`、`CON.md`、`CON.tar.md` 在
    // Windows 上是同一个设备。大小写不敏感。
    let stem = raw.split('.').next().unwrap_or(raw);
    if RESERVED_STEMS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(stem))
    {
        return Err(invalid(
            raw,
            format!("「{stem}」是 Windows 保留设备名（CON / PRN / AUX / NUL / COM1-9 / LPT1-9），不能用作文件名"),
        ));
    }

    // 全角扩展名：`foo.ｍｄ` 的尾巴全是全角字母，补 `.md` 会得到
    // `foo.ｍｄ.md`——名字难看还是小事，"用户以为文件名是 foo.ｍｄ"才是问题。
    if let Some((_, tail)) = raw.rsplit_once('.') {
        if tail
            .chars()
            .any(|ch| ('\u{ff01}'..='\u{ff5e}').contains(&ch))
        {
            return Err(invalid(
                raw,
                "扩展名里有全角字符（例如 .ｍｄ）；扫描器只认半角的 .md / .markdown",
            ));
        }
    }

    let name = if has_note_extension(raw) {
        raw.to_owned()
    } else {
        format!("{raw}.md")
    };

    // 补完之后仍然"不是笔记"的名字，正常路径上已经不可达（上面的规则堵死了
    // 空名、尾点、前导点）。留着是因为一旦它变得可达，后果是**文件建出来但
    // 树里永远看不见**——最难自查的一类失败，值得用三行换一个显式报错。
    if !has_note_extension(&name) {
        return Err(invalid(raw, "这个名字不会被扫描器识别成笔记"));
    }

    let bytes = name.len();
    if bytes > MAX_NAME_BYTES {
        return Err(invalid(
            raw,
            format!("名字太长：{bytes} 字节，上限 {MAX_NAME_BYTES} 字节（中文一个字算 3 字节）"),
        ));
    }

    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_md_only_when_missing() {
        assert_eq!(normalize_note_name("周会").unwrap(), "周会.md");
        assert_eq!(normalize_note_name("周会.md").unwrap(), "周会.md");
        assert_eq!(
            normalize_note_name("周会.markdown").unwrap(),
            "周会.markdown"
        );
        // 扩展名大小写不敏感：扫描器认 `.MD`，这里就不能补成 `.MD.md`。
        assert_eq!(normalize_note_name("A.MD").unwrap(), "A.MD");
        assert_eq!(normalize_note_name("a.Markdown").unwrap(), "a.Markdown");
        // 只补一次：`a.txt` 不是笔记名，补成 `a.txt.md` 而不是继续猜。
        assert_eq!(normalize_note_name("a.txt").unwrap(), "a.txt.md");
    }

    #[test]
    fn rejects_reserved_device_names_with_and_without_extension() {
        for input in [
            "CON",
            "con.md",
            "PRN",
            "aux.markdown",
            "NUL.md",
            "COM1",
            "lpt9.md",
        ] {
            let err = normalize_note_name(input).expect_err(input);
            assert!(
                format!("{err:?}").contains("InvalidName"),
                "{input} → {err:?}"
            );
        }
        // 名字里**包含**保留名但不是整段，是合法的。
        assert!(normalize_note_name("CONSOLE.md").is_ok());
        assert!(normalize_note_name("我的 CON.md").is_ok());
    }

    #[test]
    fn rejects_the_byte_limit_at_the_boundary() {
        let ok = "a".repeat(MAX_NAME_BYTES - 3);
        assert_eq!(normalize_note_name(&ok).unwrap().len(), MAX_NAME_BYTES);

        let too_long = "a".repeat(MAX_NAME_BYTES - 2);
        assert!(normalize_note_name(&too_long).is_err());
    }

    /// 全角扩展名与全角结构字符必须**显式拒绝**，错误里要说明为什么。
    ///
    /// 静默补一个 `.md` 也能得到可用的文件，但用户以为自己建的是 `foo.ｍｄ`，
    /// 下一次按名字找就找不到了。
    #[test]
    fn rejects_fullwidth_lookalikes_with_a_readable_reason() {
        for input in ["foo.ｍｄ", "foo.ＭＤ", "foo．md", "笔记。md", "a：b"] {
            let err = normalize_note_name(input).expect_err(input);
            let reason = match &err {
                KiError::InvalidName { reason, .. } => reason.clone(),
                other => panic!("{input} 应当是 InvalidName，实际 {other:?}"),
            };
            assert!(
                reason.contains("全角"),
                "{input} 的错误必须点明全角，实际：{reason}"
            );
        }
        // 全角逗号、全角括号不在名单里：中文名里出现它们是正常的。
        assert!(normalize_note_name("周会，第一次.md").is_ok());
        assert!(normalize_note_name("预算（2026）.md").is_ok());
    }

    /// 隐藏文件：资源管理器里看不见，而树里看得见——两边答案不一致，拒绝。
    #[test]
    fn rejects_leading_dot_names() {
        for input in [".md", ".hidden.md", ".", "..", "../x"] {
            assert!(normalize_note_name(input).is_err(), "{input} 应当被拒绝");
        }
    }
}
