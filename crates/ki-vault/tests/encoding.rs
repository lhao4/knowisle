//! 编码矩阵：12 个夹具 × 「扫描看得到 / 读得出来 / 解出什么」，外加保存的往返契约。
//!
//! 这 12 份夹具就是中文 Windows 上真实会遇到的全部形态（记事本的 ANSI /
//! Unicode / UTF-8 三件套，加上 Big5、Latin-1、空文件、只有 BOM）。
//!
//! # 为什么矩阵要住在版本库里
//!
//! 「支持 GBK」这种话没有判据。有判据的是这张表：每一行都是**某一种字节
//! 序列**在这套代码下的确定行为。改编码探测的人只要让表里任何一格变了，
//! diff 就会说话——而不是等用户拿一篇打不开的笔记来问。
//!
//! 字节一律用 `&[u8]` 字面量写：Rust 的 `str` 是 UTF-8，造不出 GBK。
//!
//! # 保存那半边同样有契约
//!
//! 读对了不算完：用户改一个字再保存，磁盘上必须还是原来那个编码。
//! 那几条用例在文件末尾，判据是**磁盘字节**，不是 `read_note` 读回来的正文
//! ——用后者验前者是拿自己的解码器证明自己的编码器。

use std::fs;
use std::path::Path;
use tempfile::TempDir;

use ki_core::KiError;
use ki_vault::scan::{flatten_files, read_note, scan_vault, ScanOptions};
use ki_vault::write_note;

/// 一份夹具：文件名、磁盘字节、期望读到的正文与探测到的编码。
struct Fixture {
    name: &'static str,
    bytes: &'static [u8],
    /// 期望的正文；`None` = 这份夹具应当读不出来。
    text: Option<&'static str>,
    /// 期望的 `decoded_from`：`None` = 磁盘上本来就是 UTF-8。
    encoding: Option<&'static str>,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "utf8_nobom",
        bytes: b"# \xe6\xa0\x87\xe9\xa2\x98\n\xe6\xad\xa3\xe6\x96\x87\n",
        text: Some("# 标题\n正文\n"),
        encoding: None,
    },
    Fixture {
        // 记事本的「UTF-8」选项：BOM 要剥掉，但它**不是**转码，不该弹提示。
        name: "utf8_bom",
        bytes: b"\xef\xbb\xbf# \xe6\xa0\x87\xe9\xa2\x98\n\xe6\xad\xa3\xe6\x96\x87\n",
        text: Some("# 标题\n正文\n"),
        encoding: None,
    },
    Fixture {
        // 记事本的「Unicode」选项。
        name: "utf16le_bom",
        bytes: &[
            0xFF, 0xFE, 0x23, 0x00, 0x20, 0x00, 0x07, 0x68, 0x98, 0x98, 0x0D, 0x00, 0x0A, 0x00,
            0x63, 0x6B, 0x87, 0x65, 0x0D, 0x00, 0x0A, 0x00,
        ],
        text: Some("# 标题\r\n正文\r\n"),
        encoding: Some("UTF-16LE"),
    },
    Fixture {
        name: "utf16be_bom",
        bytes: &[
            0xFE, 0xFF, 0x00, 0x23, 0x00, 0x20, 0x68, 0x07, 0x98, 0x98, 0x00, 0x0D, 0x00, 0x0A,
            0x6B, 0x63, 0x65, 0x87, 0x00, 0x0D, 0x00, 0x0A,
        ],
        text: Some("# 标题\r\n正文\r\n"),
        encoding: Some("UTF-16BE"),
    },
    Fixture {
        // 无 BOM 的 UTF-16LE 纯 ASCII：字节是**合法 UTF-8**（NUL < 0x80），
        // 所以它必须在 `from_utf8` **之前**被 NUL 模式拦下来。改动前它会
        // 静默读成 `h\0e\0l\0l\0o\0\n\0`，不报错也不提示。
        name: "utf16le_nobom_ascii",
        bytes: &[
            0x68, 0x00, 0x65, 0x00, 0x6C, 0x00, 0x6C, 0x00, 0x6F, 0x00, 0x0A, 0x00,
        ],
        text: Some("hello\n"),
        encoding: Some("UTF-16LE"),
    },
    Fixture {
        // 全中文、只有 6 字节：NUL 比例够不着 `sniff_utf16` 的阈值，
        // 由「猜」那一步接住。
        name: "utf16le_nobom_cjk",
        bytes: &[0x2D, 0x4E, 0x87, 0x65, 0x0A, 0x00],
        text: Some("中文\n"),
        encoding: Some("UTF-16LE"),
    },
    Fixture {
        name: "gbk",
        bytes: &[0xD6, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x0A],
        text: Some("中文笔记\n"),
        encoding: Some("GBK"),
    },
    Fixture {
        // 含 GB18030 四字节序列（𝄞 = 94 32 BE 34）：必须记成 GB18030，
        // 否则保存时会拿 GBK 的编码器误拒。
        name: "gb18030",
        bytes: &[
            0xD6, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x94, 0x32, 0xBE, 0x34, 0x0A,
        ],
        text: Some("中文笔记𝄞\n"),
        encoding: Some("GB18030"),
    },
    Fixture {
        name: "big5",
        bytes: &[0xA4, 0xA4, 0xA4, 0xE5, 0xB5, 0xA7, 0xB0, 0x4F, 0x0A],
        text: Some("中文筆記\n"),
        encoding: Some("Big5"),
    },
    Fixture {
        // 「café」：0xE9 是 Latin-1 的 é。WHATWG 的 `latin1` 标签指向
        // windows-1252，两者在 0xA0–0xFF 上一致，所以名字按前者报。
        name: "latin1",
        bytes: &[0x63, 0x61, 0x66, 0xE9, 0x0A],
        text: Some("café\n"),
        encoding: Some("Windows-1252"),
    },
    Fixture {
        name: "empty",
        bytes: b"",
        text: Some(""),
        encoding: None,
    },
    Fixture {
        name: "bom_only",
        bytes: b"\xef\xbb\xbf",
        text: Some(""),
        encoding: None,
    },
];

fn fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    for fixture in FIXTURES {
        fs::write(
            dir.path().join(format!("{}.md", fixture.name)),
            fixture.bytes,
        )
        .unwrap();
    }
    dir
}

#[test]
fn the_twelve_fixtures_read_as_measured() {
    let dir = fixture_dir();
    let root = dir.path();

    // 扫描层只认扩展名与路径，不读正文——12 份夹具全都该出现在树里。
    let scanned = scan_vault(root, &ScanOptions::default()).unwrap();
    let files: Vec<String> = flatten_files(&scanned.tree)
        .into_iter()
        .map(|file| file.path)
        .collect();
    assert_eq!(files.len(), FIXTURES.len(), "树里应当有全部夹具：{files:?}");
    assert_eq!(scanned.skipped, 0, "扫描层不读正文，不该跳过任何一份");

    for fixture in FIXTURES {
        let path = format!("{}.md", fixture.name);
        let Some(want) = fixture.text else {
            let err = read_note(Path::new(root), &path).expect_err("本应读不出来");
            assert!(
                format!("{err:?}").contains("InvalidEncoding"),
                "{} 应当报 InvalidEncoding，实际 {err:?}",
                fixture.name
            );
            continue;
        };
        let note =
            read_note(Path::new(root), &path).unwrap_or_else(|e| panic!("{}: {e:?}", fixture.name));
        assert_eq!(note.content, want, "{} 的正文", fixture.name);
        assert_eq!(
            note.decoded_from.as_deref(),
            fixture.encoding,
            "{} 的 decoded_from",
            fixture.name
        );
    }
}

/// 空文件与「只有 BOM」必须都是空正文，而不是报错或留下 U+FEFF。
#[test]
fn empty_and_bom_only_are_both_empty() {
    let dir = fixture_dir();
    for name in ["empty.md", "bom_only.md"] {
        let note = read_note(dir.path(), name).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(note.content, "", "{name} 的正文必须为空");
        assert_eq!(note.decoded_from, None, "{name} 不该被当成转码");
    }
}

/* ───────────────── 保存：写回原编码 ───────────────── */

/// 磁盘上的原始字节。判据必须看它，不能看 `read_note` 读回来的正文。
fn disk_bytes(root: &Path, name: &str) -> Vec<u8> {
    fs::read(root.join(name)).unwrap()
}

/// GBK 笔记改一个字再保存，磁盘上必须还是 GBK，且只有那一个字变了。
#[test]
fn gbk_note_saves_back_as_gbk() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    // 「中文笔记\n」
    let original = [0xD6, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x0A];
    fs::write(root.join("gbk.md"), original).unwrap();

    let note = read_note(root, "gbk.md").unwrap();
    let edited = note.content.replace("笔记", "筆記");
    write_note(root, "gbk.md", &edited).expect("GBK 表示得了这几个字，应当能存");

    let after = disk_bytes(root, "gbk.md");
    // 期望值由 `iconv` 生成，不是自己算的：拿自己的编码器当判据，
    // 编错了也照样绿。
    assert_eq!(after, gbk_bytes("中文筆記\n"));
    assert_ne!(after, edited.as_bytes(), "写回去的不能是 UTF-8");
}

/// GBK 装不下 emoji：拒绝保存，磁盘一个字节都不动。
#[test]
fn gbk_note_refuses_emoji_and_leaves_disk_untouched() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let original = [0xD6, 0xD0, 0xCE, 0xC4, 0x0A]; // 「中文\n」
    fs::write(root.join("gbk.md"), original).unwrap();

    let note = read_note(root, "gbk.md").unwrap();
    let err = write_note(root, "gbk.md", &format!("{}😀", note.content))
        .expect_err("GBK 装不下 emoji，必须拒绝");

    match &err {
        KiError::EncodingNotWritable {
            encoding, reason, ..
        } => {
            assert_eq!(encoding, "GBK");
            assert!(reason.contains('😀'), "理由里要说清是哪个字符：{reason}");
        }
        other => panic!("应当是 EncodingNotWritable，实际 {other:?}"),
    }
    assert_eq!(disk_bytes(root, "gbk.md"), original, "拒绝保存时磁盘不许动");
}

/// 带 BOM 的 UTF-8 保存后必须还带 BOM——否则第一次保存就悄悄改了文件头。
#[test]
fn utf8_bom_is_preserved_on_save() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(
        root.join("bom.md"),
        b"\xef\xbb\xbf# \xe6\xa0\x87\xe9\xa2\x98\n",
    )
    .unwrap();

    write_note(root, "bom.md", "# 标题\n正文\n").unwrap();

    let after = disk_bytes(root, "bom.md");
    assert_eq!(&after[..3], &[0xEF, 0xBB, 0xBF], "BOM 必须写回去");
    assert_eq!(&after[3..], "# 标题\n正文\n".as_bytes());
}

/// UTF-16 笔记保存后仍是 UTF-16，且 BOM 在。
#[test]
fn utf16_note_saves_back_as_utf16() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let mut original = vec![0xFF, 0xFE];
    original.extend("# 标题\n".encode_utf16().flat_map(|u| u.to_le_bytes()));
    fs::write(root.join("u16.md"), &original).unwrap();

    write_note(root, "u16.md", "# 标题\n正文\n").unwrap();

    let after = disk_bytes(root, "u16.md");
    assert_eq!(&after[..2], &[0xFF, 0xFE], "UTF-16LE 的 BOM 必须写回去");
    let decoded = read_note(root, "u16.md").unwrap();
    assert_eq!(decoded.content, "# 标题\n正文\n");
    assert_eq!(decoded.decoded_from.as_deref(), Some("UTF-16LE"));
}

/// 新建的笔记是 UTF-8，不带 BOM。
#[test]
fn new_note_is_plain_utf8() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    write_note(root, "new.md", "# 新\n").unwrap();
    assert_eq!(disk_bytes(root, "new.md"), "# 新\n".as_bytes());
}

/// Big5 有重复映射：`A2 A4` 与 `F9 F9` 都是「═」。这种文件写回去必然改字节，
/// 所以只读——§3 的「不可逆就退化成只读」在这里有真实触发点。
#[test]
fn big5_duplicate_mapping_is_read_only() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    // 「中文筆記」+ 一个重复映射的「═」
    let original = [0xA4, 0xA4, 0xA4, 0xE5, 0xB5, 0xA7, 0xB0, 0x4F, 0xA2, 0xA4];
    fs::write(root.join("big5.md"), original).unwrap();

    let note = read_note(root, "big5.md").unwrap();
    assert_eq!(note.content, "中文筆記═");
    assert_eq!(note.decoded_from.as_deref(), Some("Big5"));

    let err = write_note(root, "big5.md", &note.content).expect_err("往返不无损时必须拒绝保存");
    assert!(
        format!("{err:?}").contains("EncodingNotWritable"),
        "实际 {err:?}"
    );
    assert_eq!(
        disk_bytes(root, "big5.md"),
        original,
        "拒绝保存时磁盘不许动"
    );
}

/// 改一个字之后正文里的其他字符没被吃掉（防「只写回编码、内容却丢字」）。
#[test]
fn gbk_edit_keeps_the_rest_of_the_note() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let text = "# 会议纪要\n\n今天讨论了三件事：索引重建的时机、编码探测的顺序、以及保存时如何写回原编码。\n";
    let bytes = gbk_bytes(text);
    fs::write(root.join("gbk.md"), &bytes).unwrap();

    let note = read_note(root, "gbk.md").unwrap();
    assert_eq!(note.content, text);

    let edited = text.replace("三件事", "两件事");
    write_note(root, "gbk.md", &edited).unwrap();
    assert_eq!(disk_bytes(root, "gbk.md"), gbk_bytes(&edited));
}

/// 用 `iconv` 生成 GBK 字节：**不拿自己的编码器当夹具的裁判**。
fn gbk_bytes(text: &str) -> Vec<u8> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("iconv")
        .args(["-f", "UTF-8", "-t", "GBK"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("这些用例需要 iconv（git-bash 自带）");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "iconv 失败");
    out.stdout
}
