//! 编码探测与转码：让知屿打得开「别人的笔记」，且不偷偷改写它们。
//!
//! # 为什么需要这个模块
//!
//! 中文 Windows 上三种「记事本」产物都会落在 Vault 里：
//!
//! | 记事本的选项 | 磁盘上是什么 | 改动前 |
//! |---|---|---|
//! | ANSI | GBK（CP936） | `InvalidEncoding`，既打不开也搜不到 |
//! | Unicode | UTF-16LE + BOM | 同上 |
//! | UTF-8 | UTF-8 + BOM | 能读（BOM 被剥掉） |
//!
//! 还有第四种更阴的：**UTF-16LE 不带 BOM 且正文是纯 ASCII**。它的字节
//! （`68 00 65 00 ...`）是**合法 UTF-8**——NUL 是合法 UTF-8 字符——于是
//! 改动前它会「成功」读成 `h\0e\0l\0l\0o\0`，不报错、不提示，用户看到的
//! 是一串带方框的乱码。这一条是整个模块存在的理由：**探测必须发生在
//! `from_utf8` 之前**，否则那条分支永远走不到。
//!
//! # 顺序是契约，不是实现细节
//!
//! ```text
//! BOM → NUL 模式（UTF-16/32）→ 严格 UTF-8 → 猜（chardetng）→ GB18030 → 认输
//! ```
//!
//! - **BOM 优先**：它是文件自己声明的，比任何统计都可靠。
//! - **NUL 模式第二**：必须在严格 UTF-8 之前，理由见上。只看**模式**
//!   （某一奇偶位上大面积为 NUL），不看「有没有 NUL」——一个 UTF-8 文件里
//!   偶然出现一个 NUL 不该让它被当成 UTF-16。
//! - **严格 UTF-8 第三**：这是 99.99% 的路径，也是**唯一**能保证「不改动
//!   一个字节」的路径。探测一步都不许插到它前面。
//! - **猜**只在严格 UTF-8 失败之后跑。所以「把好的 UTF-8 猜成别的编码」
//!   在结构上不可能发生——这是 §0.5② 那条「误判率 0」的兑现方式，
//!   不是靠调参。
//!
//! # 为什么不用 `from_utf8_lossy`
//!
//! 它把「解不出来」变成「一堆 U+FFFD」，而 U+FFFD 会被原样写回磁盘。
//! 那是拿用户的数据换一个不报错的假象。本模块的每一条解码路径都是
//! **要么完整解出来、要么返回 `None`**（`decode_without_bom_handling_and_without_replacement`）。
//!
//! # 保存为什么必须回到原编码
//!
//! 用户打开一篇 GBK 笔记，改一个字，保存——如果写回 UTF-8，这篇笔记在
//! 他其他的工具里（老版本编辑器、某些同步盘预览、他自己的脚本）就坏了。
//! 我们无权替他做这个决定，所以 [`encode`] 把正文编回原编码；编不回去
//! （GBK 装不下 emoji）就**拒绝保存**，而不是偷偷换成 UTF-8。

use encoding_rs::{BIG5, GB18030, GBK, UTF_16BE, UTF_16LE, WINDOWS_1252};

/// 磁盘上一篇笔记的编码形态。
///
/// `Utf8` 与 `Utf8Bom` 是「本来就是 UTF-8」的两种形态：它们不产生
/// `NoteContent.decoded_from`（前端不该为它们显示提示），但保存时要能
/// 分辨——带 BOM 的文件必须把 BOM 写回去，否则第一次保存就悄悄改了文件头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// 无 BOM 的 UTF-8。
    Utf8,
    /// UTF-8 + BOM（记事本的「UTF-8」选项）。
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
    /// GBK（CP936）。**编码器**比 GB18030 窄：装不下的字符直接拒绝。
    Gbk,
    /// GB18030。比 GBK 多一层四字节序列，覆盖全部 Unicode。
    Gb18030,
    Big5,
    /// windows-1252（WHATWG 的 `latin1` 标签指向它，不是 ISO-8859-1）。
    Windows1252,
}

impl Encoding {
    /// 给人看的名字。同时是 `NoteContent.decoded_from` 的取值集合——
    /// 前端只拿它拼一句话，不据此分支，所以是字符串而不是又一个枚举
    /// （枚举会逼前端再写一张标签表，两张表迟早会漂）。
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 (BOM)",
            Encoding::Utf16Le => "UTF-16LE",
            Encoding::Utf16Be => "UTF-16BE",
            Encoding::Utf32Le => "UTF-32LE",
            Encoding::Utf32Be => "UTF-32BE",
            Encoding::Gbk => "GBK",
            Encoding::Gb18030 => "GB18030",
            Encoding::Big5 => "Big5",
            Encoding::Windows1252 => "Windows-1252",
        }
    }

    /// 是不是「本来就是 UTF-8」。
    ///
    /// BOM 也算：它是文件头的一种写法，不是另一种字符集。所以它不该触发
    /// 「已按 X 解码」的提示——那会让用户以为自己的文件被转过码。
    pub fn is_utf8(self) -> bool {
        matches!(self, Encoding::Utf8 | Encoding::Utf8Bom)
    }
}

/// 解码结果。
#[derive(Debug, Clone)]
pub struct Decoded {
    pub text: String,
    pub encoding: Encoding,
}

impl Decoded {
    /// 「这篇是从别的编码转过来的」时给出编码名，否则 `None`。
    ///
    /// 这就是 `NoteContent.decoded_from` 的唯一来源：`None` 严格等价于
    /// 「磁盘上本来就是 UTF-8」。
    pub fn transcoded_from(&self) -> Option<&'static str> {
        (!self.encoding.is_utf8()).then(|| self.encoding.label())
    }
}

/// 解码失败：认不出编码，或者认出来了但字节在该编码下不合法。
///
/// 只有一个变体是刻意的——对用户来说下一步动作完全一样（这篇打不开），
/// 而「试过哪些编码」属于日志，不属于错误契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Undecodable;

/// 把磁盘上的字节解成正文。
///
/// 收 `Vec<u8>` 而不是 `&[u8]`：UTF-8 快路径上 `String::from_utf8` 直接
/// 复用这块缓冲区，10k 篇建库一次多余的拷贝都不做。
pub fn decode(bytes: Vec<u8>) -> Result<Decoded, Undecodable> {
    let encoding = detect(&bytes).ok_or(Undecodable)?;
    if encoding == Encoding::Utf8 {
        // `detect` 已经验过一遍，这里再验一遍是 `String::from_utf8` 的
        // 固有行为。两遍 3.9 MB 的 UTF-8 校验在 10k 建库上是毫秒级，
        // 换来「判定顺序只有一份」——这笔买卖值得。
        return String::from_utf8(bytes)
            .map(|text| Decoded { text, encoding })
            .map_err(|_| Undecodable);
    }
    let text = decode_with(&bytes, encoding).ok_or(Undecodable)?;
    Ok(Decoded { text, encoding })
}

/// 探测编码，不产出正文。
///
/// 保存路径用它：先知道「这篇原来是什么编码」，才能选对编码器。
/// 与 [`decode`] 共用同一份判定顺序，不存在「读的时候判成 A、写的时候判成 B」。
pub fn detect(bytes: &[u8]) -> Option<Encoding> {
    if let Some(encoding) = detect_bom(bytes) {
        return decode_with(bytes, encoding).map(|_| encoding);
    }

    // NUL 模式必须在 `from_utf8` **之前**：无 BOM 的 UTF-16LE 纯 ASCII
    // 是合法 UTF-8，放到后面就永远走不到这里。
    if let Some(encoding) = sniff_utf16(bytes) {
        if decode_with(bytes, encoding).is_some_and(|t| is_plausible(&t)) {
            return Some(encoding);
        }
    }

    if std::str::from_utf8(bytes).is_ok() {
        return Some(Encoding::Utf8);
    }

    guess(bytes).ok().map(|decoded| decoded.encoding)
}

/// 原字节在该编码下能否**无损往返**：`decode(bytes) → encode → bytes`。
///
/// 这是保存前的那道闸（§3）：往返不成立时写回去就是在改用户的字节，
/// 哪怕正文看起来一样。Big5 的重复映射（同一个字有两个码位）就会踩到它。
pub fn is_lossless(bytes: &[u8], encoding: Encoding) -> bool {
    match decode_with(bytes, encoding) {
        Some(text) => encode(&text, encoding).is_ok_and(|back| back == bytes),
        None => false,
    }
}

/// 按指定编码编码正文。
///
/// 失败即「不能写」：调用方必须拒绝保存，而不是退化成 UTF-8。
pub fn encode(text: &str, encoding: Encoding) -> Result<Vec<u8>, EncodeError> {
    let bytes = match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf8Bom => {
            let mut out = Vec::with_capacity(text.len() + 3);
            out.extend_from_slice(&BOM_UTF8);
            out.extend_from_slice(text.as_bytes());
            out
        }
        // UTF-16 / UTF-32 **必须自己写**。Encoding Standard 里它们只是
        // 解码编码（decode-only），`Encoding::encode` 会**静默退回 UTF-8**
        // 并在返回值里把 `used` 报成 UTF-8——实测如此。信了它就是把一篇
        // UTF-16 笔记写成 UTF-8，正是本刀要杜绝的那种静默转码。
        Encoding::Utf16Le | Encoding::Utf16Be => encode_utf16(text, encoding == Encoding::Utf16Be),
        Encoding::Utf32Le | Encoding::Utf32Be => encode_utf32(text, encoding == Encoding::Utf32Be),
        other => {
            let codec = codec(other);
            let (bytes, used, had_errors) = codec.encode(text);
            // 第二道保险：即使将来 encoding_rs 又给某个编码加了「退回 UTF-8」
            // 的行为，这里也会当场发现，而不是把它写进用户的文件。
            debug_assert_eq!(used, codec, "{} 不是输出编码", other.label());
            if had_errors {
                // encoding_rs 会把装不下的字符换成 `&#128512;` 这样的
                // 数字字符引用——那比丢字更坏：文件看起来是好的，内容却
                // 被偷偷改写了。所以这里必须转成硬错误。
                return Err(EncodeError::Unmappable {
                    encoding,
                    ch: first_unmappable(text, codec),
                });
            }
            bytes.into_owned()
        }
    };

    // §3 的第二道闸：编出来的字节解回去必须还是原文。
    // UTF-8 两种形态由构造保证（逐字节原样），不必再跑一遍。
    if !encoding.is_utf8() {
        match decode_with(&bytes, encoding) {
            Some(back) if back == text => {}
            _ => return Err(EncodeError::NotRoundTrippable { encoding }),
        }
    }

    Ok(bytes)
}

/// 编码失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// 正文里有这个编码表示不了的字符。
    Unmappable {
        encoding: Encoding,
        ch: Option<char>,
    },
    /// 编出来的字节解不回原文（原文件里存在该编码的重复映射）。
    NotRoundTrippable { encoding: Encoding },
}

// ---------------------------------------------------------------------------
// 探测
// ---------------------------------------------------------------------------

/// UTF-8 BOM。记事本「UTF-8」选项的产物。
const BOM_UTF8: [u8; 3] = [0xEF, 0xBB, 0xBF];

fn detect_bom(bytes: &[u8]) -> Option<Encoding> {
    // UTF-32 必须排在 UTF-16 之前：`FF FE 00 00` 的前两字节也是 UTF-16LE
    // 的 BOM，先匹配 UTF-16 会把一篇 UTF-32 解成一堆 NUL 开头的垃圾。
    if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        return Some(Encoding::Utf32Le);
    }
    if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        return Some(Encoding::Utf32Be);
    }
    if bytes.starts_with(&BOM_UTF8) {
        return Some(Encoding::Utf8Bom);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(Encoding::Utf16Le);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(Encoding::Utf16Be);
    }
    None
}

/// 无 BOM 的 UTF-16 嗅探：看 NUL 是不是**成片地**落在同一个奇偶位上。
///
/// # 阈值为什么是 50% / 12.5%
///
/// UTF-16LE 的纯 ASCII 正文，奇数位上全是 NUL（100%）；含中文时比例会降，
/// 但中文的 UTF-16 低字节很少是 0，所以只要正文里有几行英文就还在 50% 以上。
/// 另一半的上限是防「两边都有 NUL」的二进制垃圾被硬判成某一头。
///
/// 这个函数**只认模式不认内容**：一个 UTF-8 文件里偶然有个 NUL 时，
/// 两个比例都远低于阈值，于是原样走严格 UTF-8，不会被误判。
fn sniff_utf16(bytes: &[u8]) -> Option<Encoding> {
    if bytes.len() < 4 || bytes.len() % 2 != 0 {
        return None;
    }
    let pairs = bytes.len() / 2;
    let mut even = 0usize;
    let mut odd = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == 0 {
            if index % 2 == 0 {
                even += 1;
            } else {
                odd += 1;
            }
        }
    }

    let dominant = |a: usize, b: usize| a * 2 >= pairs && b * 8 <= pairs;
    if dominant(odd, even) {
        return Some(Encoding::Utf16Le);
    }
    if dominant(even, odd) {
        return Some(Encoding::Utf16Be);
    }
    None
}

/// 严格 UTF-8 失败之后的兜底：猜一次，猜不出来再按 GB18030（它覆盖全
/// Unicode，永不失败）。
///
/// 每一步都要过 [`is_plausible`]：猜错的代价是乱码，但**不许**把乱码
/// 变成「看起来成功」——一篇 Markdown 里出现 NUL 或 C0 控制字符，那一定
/// 是猜错了。
fn guess(bytes: &[u8]) -> Result<Decoded, Undecodable> {
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    // `allow_utf8 = false`：能走到这里说明严格 UTF-8 已经失败了。
    let guessed = detector.guess(None, false);

    if let Some(text) = guessed
        .decode_without_bom_handling_and_without_replacement(bytes)
        .filter(|t| is_plausible(t))
    {
        let encoding = refine_gb(guessed, bytes);
        return Ok(Decoded {
            text: text.into_owned(),
            encoding,
        });
    }

    // GB18030 是「覆盖全 Unicode」的那张网。走到这里说明 chardetng 的答案
    // 解不通或不合理（典型：它把 GBK 猜成了 windows-1252，而后者解出来
    // 全是控制字符）。
    if let Some(text) = GB18030
        .decode_without_bom_handling_and_without_replacement(bytes)
        .filter(|t| is_plausible(t))
    {
        return Ok(Decoded {
            text: text.into_owned(),
            encoding: if has_gb18030_four_byte(bytes) {
                Encoding::Gb18030
            } else {
                Encoding::Gbk
            },
        });
    }

    // 最后试一次无 BOM 的 UTF-16：全中文的短文件没有足够的 NUL 给
    // `sniff_utf16`，但它的字节模式仍然只有 UTF-16 能解。
    if bytes.len() % 2 == 0 {
        for encoding in [Encoding::Utf16Le, Encoding::Utf16Be] {
            if let Some(text) = decode_with(bytes, encoding).filter(|t| is_plausible(t)) {
                return Ok(Decoded { text, encoding });
            }
        }
    }

    Err(Undecodable)
}

/// chardetng 把 GBK 与 GB18030 视作同一族（解码器本来就是同一个）。
/// 真正要分的是**编码器**：GBK 装不下四字节才能表示的字符，所以一篇
/// 真的含四字节序列的文件必须记成 GB18030，否则保存时会误拒。
///
/// 落到「没建模的编码」（EUC-JP、Shift_JIS…）时按 GB18030 记账：解码
/// 这一步是对的（chardetng 选的就是它），但编码器换了一个，所以
/// [`is_lossless`] 会在保存前发现字节对不上并拒绝写入。**不能**在这里
/// 假装它是 UTF-8——那才是真正的静默转码。
fn refine_gb(guessed: &'static encoding_rs::Encoding, bytes: &[u8]) -> Encoding {
    if guessed == GBK || guessed == GB18030 {
        return if has_gb18030_four_byte(bytes) {
            Encoding::Gb18030
        } else {
            Encoding::Gbk
        };
    }
    if guessed == BIG5 {
        return Encoding::Big5;
    }
    if guessed == WINDOWS_1252 {
        return Encoding::Windows1252;
    }
    Encoding::Gb18030
}

/// 字节流里有没有 GB18030 的四字节序列。
///
/// 必须**按 GBK 的码元边界走**，不能直接找「前导字节 + 0x30..0x39」：
/// 合法 GBK 的两字节组合里，第二个字节永远不会是 0x30..0x39，但它可能是
/// **下一个**组合的前导字节。逐码元走一遍最省事也最准。
fn has_gb18030_four_byte(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() {
        let lead = bytes[index];
        if lead < 0x80 {
            index += 1;
            continue;
        }
        if !(0x81..=0xFE).contains(&lead) {
            index += 1;
            continue;
        }
        match bytes.get(index + 1) {
            Some(trail) if (0x30..=0x39).contains(trail) => return true,
            Some(_) => index += 2,
            None => index += 1,
        }
    }
    false
}

/// 解出来的文本像不像一篇笔记。
///
/// 判据只有一条：**不许有 NUL 与 C0 控制字符**（`\t` `\n` `\r` 除外）。
/// Markdown 正文里不会有它们，而「把 UTF-16 当成单字节编码解」的产物
/// 一定满是 NUL——这是最便宜也最有效的一道甄别。
fn is_plausible(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c == '\0' || (c.is_control() && !matches!(c, '\t' | '\n' | '\r')))
}

// ---------------------------------------------------------------------------
// 解码
// ---------------------------------------------------------------------------

fn codec(encoding: Encoding) -> &'static encoding_rs::Encoding {
    match encoding {
        Encoding::Utf8 | Encoding::Utf8Bom => encoding_rs::UTF_8,
        Encoding::Utf16Le => UTF_16LE,
        Encoding::Utf16Be => UTF_16BE,
        Encoding::Gbk => GBK,
        Encoding::Gb18030 => GB18030,
        Encoding::Big5 => BIG5,
        Encoding::Windows1252 => WINDOWS_1252,
        // UTF-32 由本模块自己处理，encoding_rs 没有它。
        Encoding::Utf32Le | Encoding::Utf32Be => encoding_rs::UTF_8,
    }
}

/// 严格解码：一个字节解不通就整体失败。
fn decode_with(bytes: &[u8], encoding: Encoding) -> Option<String> {
    let body = match encoding {
        Encoding::Utf8 => bytes,
        Encoding::Utf8Bom => bytes.strip_prefix(&BOM_UTF8[..])?,
        Encoding::Utf16Le => strip_bom(bytes, &[0xFF, 0xFE]),
        Encoding::Utf16Be => strip_bom(bytes, &[0xFE, 0xFF]),
        Encoding::Utf32Le => {
            return decode_utf32(strip_bom(bytes, &[0xFF, 0xFE, 0x00, 0x00]), false)
        }
        Encoding::Utf32Be => {
            return decode_utf32(strip_bom(bytes, &[0x00, 0x00, 0xFE, 0xFF]), true)
        }
        _ => bytes,
    };

    let text = codec(encoding)
        .decode_without_bom_handling_and_without_replacement(body)?
        .into_owned();

    // `decode_without_bom_handling` 不剥 BOM：UTF-16 的 `FF FE` 会变成正文
    // 开头的一个 U+FEFF。它和 UTF-8 的 BOM 一样，会让「首个标题」失效。
    Some(match text.strip_prefix('\u{feff}') {
        Some(stripped) => stripped.to_owned(),
        None => text,
    })
}

fn strip_bom<'a>(bytes: &'a [u8], bom: &[u8]) -> &'a [u8] {
    bytes.strip_prefix(bom).unwrap_or(bytes)
}

fn decode_utf32(body: &[u8], big_endian: bool) -> Option<String> {
    if body.len() % 4 != 0 {
        return None;
    }
    let mut out = String::with_capacity(body.len() / 4);
    for chunk in body.chunks_exact(4) {
        let unit = if big_endian {
            u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        } else {
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        };
        out.push(char::from_u32(unit)?);
    }
    Some(out)
}

fn encode_utf16(text: &str, big_endian: bool) -> Vec<u8> {
    let bom: [u8; 2] = if big_endian {
        [0xFE, 0xFF]
    } else {
        [0xFF, 0xFE]
    };
    let mut out = Vec::with_capacity(2 + text.len() * 2);
    out.extend_from_slice(&bom);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&if big_endian {
            unit.to_be_bytes()
        } else {
            unit.to_le_bytes()
        });
    }
    out
}

fn encode_utf32(text: &str, big_endian: bool) -> Vec<u8> {
    let bom: [u8; 4] = if big_endian {
        [0x00, 0x00, 0xFE, 0xFF]
    } else {
        [0xFF, 0xFE, 0x00, 0x00]
    };
    let mut out = Vec::with_capacity(4 + text.chars().count() * 4);
    out.extend_from_slice(&bom);
    for ch in text.chars() {
        let unit = ch as u32;
        let raw = if big_endian {
            unit.to_be_bytes()
        } else {
            unit.to_le_bytes()
        };
        out.extend_from_slice(&raw);
    }
    out
}

/// 找出第一个编不进去的字符，只为了把错误说清楚。
///
/// 只在失败路径上跑，且逐字符编码——一篇 2MB 的笔记走到这里本来就意味着
/// 保存要失败了，多花这一遍换一句「emoji 装不下」是划算的。
fn first_unmappable(text: &str, codec: &'static encoding_rs::Encoding) -> Option<char> {
    text.chars().find(|ch| {
        let mut buf = [0u8; 4];
        let (_, _, had_errors) = codec.encode(ch.encode_utf8(&mut buf));
        had_errors
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(bytes: &[u8]) -> Decoded {
        decode(bytes.to_vec()).expect("应当能解码")
    }

    /// 无 BOM 的 UTF-16LE 纯 ASCII 是合法 UTF-8——这一条是本模块存在的理由。
    #[test]
    fn utf16le_without_bom_is_detected_before_utf8() {
        let bytes = [
            0x68, 0x00, 0x65, 0x00, 0x6C, 0x00, 0x6C, 0x00, 0x6F, 0x00, 0x0A, 0x00,
        ];
        assert!(
            std::str::from_utf8(&bytes).is_ok(),
            "前提：它确实是合法 UTF-8"
        );
        let decoded = dec(&bytes);
        assert_eq!(decoded.text, "hello\n");
        assert_eq!(decoded.encoding, Encoding::Utf16Le);
    }

    /// 严格 UTF-8 必须走「零改动」的那条路。
    #[test]
    fn plain_utf8_is_not_transcoded() {
        let decoded = dec("# 标题\n正文\n".as_bytes());
        assert_eq!(decoded.encoding, Encoding::Utf8);
        assert_eq!(decoded.transcoded_from(), None);
    }

    /// BOM 剥掉，但编码仍算「原生 UTF-8」——不该弹提示。
    #[test]
    fn utf8_bom_is_stripped_but_not_a_transcode() {
        let decoded = dec(&[0xEF, 0xBB, 0xBF, b'#', b' ', 0xE6, 0xA0, 0x87]);
        assert_eq!(decoded.text, "# 标");
        assert_eq!(decoded.encoding, Encoding::Utf8Bom);
        assert_eq!(decoded.transcoded_from(), None);
        assert_eq!(
            encode(&decoded.text, Encoding::Utf8Bom).unwrap()[..3],
            [0xEF, 0xBB, 0xBF]
        );
    }

    /// 偶然出现 NUL 的 UTF-8 不许被判成 UTF-16。
    #[test]
    fn a_stray_nul_does_not_make_it_utf16() {
        let unit = "这是一篇很长的中文笔记，用来把 NUL 的比例稀释到阈值以下。";
        let mut text = String::new();
        for round in 0..4 {
            text.push_str(unit);
            // 插在字符边界上，且总量保持偶数长度——否则测的是「长度奇数
            // 直接跳过」，而不是比例阈值本身。
            if round % 2 == 1 {
                text.push('\0');
            }
        }
        let bytes = text.clone().into_bytes();
        assert_eq!(
            bytes.len() % 2,
            0,
            "偶数长度，让 sniff_utf16 真的有机会判它"
        );
        let decoded = dec(&bytes);
        assert_eq!(decoded.encoding, Encoding::Utf8);
        assert_eq!(decoded.text, text);
    }

    #[test]
    fn gbk_round_trips_byte_for_byte() {
        let bytes = vec![0xD6, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x0A];
        let decoded = dec(&bytes);
        assert_eq!(decoded.text, "中文笔记\n");
        assert_eq!(decoded.encoding, Encoding::Gbk);
        assert_eq!(encode(&decoded.text, decoded.encoding).unwrap(), bytes);
    }

    /// GBK 装不下 emoji：必须报错，而不是写成 `&#128512;`。
    #[test]
    fn gbk_refuses_emoji() {
        let err = encode("中文😀", Encoding::Gbk).expect_err("GBK 装不下 emoji");
        assert_eq!(
            err,
            EncodeError::Unmappable {
                encoding: Encoding::Gbk,
                ch: Some('😀')
            }
        );
    }

    /// GB18030 装得下 emoji，且能解回原文。
    #[test]
    fn gb18030_can_encode_emoji() {
        let bytes = encode("中文😀", Encoding::Gb18030).unwrap();
        assert_eq!(decode(bytes).unwrap().text, "中文😀");
    }

    /// 含四字节序列的文件必须记成 GB18030，否则保存时会误拒。
    #[test]
    fn four_byte_sequences_are_gb18030() {
        let bytes = vec![0x94, 0x32, 0xBE, 0x34];
        let decoded = dec(&bytes);
        assert_eq!(decoded.text, "𝄞");
        assert_eq!(decoded.encoding, Encoding::Gb18030);
    }

    #[test]
    fn utf16_bom_round_trips() {
        let text = "# 标题\r\n正文\r\n";
        for encoding in [Encoding::Utf16Le, Encoding::Utf16Be] {
            let bytes = encode(text, encoding).unwrap();
            let decoded = decode(bytes.clone()).unwrap();
            assert_eq!(decoded.text, text, "{}", encoding.label());
            assert_eq!(decoded.encoding, encoding);
            assert_eq!(encode(&decoded.text, encoding).unwrap(), bytes);
        }
    }

    #[test]
    fn utf32_bom_round_trips() {
        let text = "# 标题 𝄞\n";
        for encoding in [Encoding::Utf32Le, Encoding::Utf32Be] {
            let bytes = encode(text, encoding).unwrap();
            let decoded = decode(bytes.clone()).unwrap();
            assert_eq!(decoded.text, text, "{}", encoding.label());
            assert_eq!(encode(&decoded.text, encoding).unwrap(), bytes);
        }
    }

    /// UTF-32 的 BOM 前两字节与 UTF-16LE 相同，顺序不能换。
    #[test]
    fn utf32_bom_wins_over_utf16_bom() {
        let bytes = encode("A", Encoding::Utf32Le).unwrap();
        assert_eq!(decode(bytes).unwrap().encoding, Encoding::Utf32Le);
    }

    /// 真正认不出来的字节要报错，而不是猜成 windows-1252 蒙混过去。
    ///
    /// 判据是「解出来有没有控制字符」：windows-1252 能解几乎任何字节，
    /// 但一篇正文里出现 `U+0001` 一定是猜错了。
    #[test]
    fn undecodable_bytes_are_reported() {
        assert_eq!(decode(vec![0x01, 0xFF, 0x02]).unwrap_err(), Undecodable);
    }
}
