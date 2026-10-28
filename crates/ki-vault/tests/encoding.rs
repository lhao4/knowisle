//! 编码矩阵：12 个夹具 × 「扫描看得到 / 读得出来 / 解出什么」。
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

use std::fs;
use std::path::Path;

use ki_vault::scan::{flatten_files, read_note, scan_vault, ScanOptions};

/// 一份夹具：文件名、磁盘字节、期望读到的正文。
struct Fixture {
    name: &'static str,
    bytes: &'static [u8],
    /// `Ok(text)` = 读得出来且正文是它；`Err(())` = 报 `InvalidEncoding`。
    expect: Result<&'static str, ()>,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "utf8_nobom",
        bytes: b"# \xe6\xa0\x87\xe9\xa2\x98\n\xe6\xad\xa3\xe6\x96\x87\n",
        expect: Ok("# 标题\n正文\n"),
    },
    Fixture {
        name: "utf8_bom",
        bytes: b"\xef\xbb\xbf# \xe6\xa0\x87\xe9\xa2\x98\n\xe6\xad\xa3\xe6\x96\x87\n",
        expect: Ok("# 标题\n正文\n"),
    },
    Fixture {
        name: "utf16le_bom",
        bytes: &[
            0xFF, 0xFE, 0x23, 0x00, 0x20, 0x00, 0x07, 0x68, 0x98, 0x98, 0x0D, 0x00, 0x0A, 0x00,
            0x63, 0x6B, 0x87, 0x65, 0x0D, 0x00, 0x0A, 0x00,
        ],
        expect: Err(()),
    },
    Fixture {
        name: "utf16be_bom",
        bytes: &[
            0xFE, 0xFF, 0x00, 0x23, 0x00, 0x20, 0x68, 0x07, 0x98, 0x98, 0x00, 0x0D, 0x00, 0x0A,
            0x6B, 0x63, 0x65, 0x87, 0x00, 0x0D, 0x00, 0x0A,
        ],
        expect: Err(()),
    },
    Fixture {
        // 无 BOM 的 UTF-16LE 纯 ASCII：字节是**合法 UTF-8**，改动前会静默
        // 读成一串带 NUL 的串，不报错也不提示。
        name: "utf16le_nobom_ascii",
        bytes: &[0x68, 0x00, 0x65, 0x00, 0x6C, 0x00, 0x6C, 0x00, 0x6F, 0x00, 0x0A, 0x00],
        expect: Ok("h\0e\0l\0l\0o\0\n\0"),
    },
    Fixture {
        name: "utf16le_nobom_cjk",
        bytes: &[0x2D, 0x4E, 0x87, 0x65, 0x0A, 0x00],
        expect: Err(()),
    },
    Fixture {
        name: "gbk",
        bytes: &[0xD6, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x0A],
        expect: Err(()),
    },
    Fixture {
        name: "gb18030",
        bytes: &[
            0xD6, 0xD0, 0xCE, 0xC4, 0xB1, 0xCA, 0xBC, 0xC7, 0x94, 0x32, 0xBE, 0x34, 0x0A,
        ],
        expect: Err(()),
    },
    Fixture {
        name: "big5",
        bytes: &[0xA4, 0xA4, 0xA4, 0xE5, 0xB5, 0xA7, 0xB0, 0x4F, 0x0A],
        expect: Err(()),
    },
    Fixture {
        name: "latin1",
        bytes: &[0x63, 0x61, 0x66, 0xE9, 0x0A],
        expect: Err(()),
    },
    Fixture {
        name: "empty",
        bytes: b"",
        expect: Ok(""),
    },
    Fixture {
        name: "bom_only",
        bytes: b"\xef\xbb\xbf",
        expect: Ok(""),
    },
];

#[test]
fn the_twelve_fixtures_read_as_measured() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    for fixture in FIXTURES {
        fs::write(root.join(format!("{}.md", fixture.name)), fixture.bytes).unwrap();
    }

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
        match (read_note(Path::new(root), &path), fixture.expect) {
            (Ok(note), Ok(want)) => assert_eq!(note.content, want, "{} 的正文", fixture.name),
            (Err(err), Err(())) => assert!(
                format!("{err:?}").contains("InvalidEncoding"),
                "{} 应当报 InvalidEncoding，实际 {err:?}",
                fixture.name
            ),
            (Ok(note), Err(())) => panic!(
                "{} 本应读不出来，实际读成了 {:?}",
                fixture.name, note.content
            ),
            (Err(err), Ok(want)) => {
                panic!("{} 本应读成 {want:?}，实际报错 {err:?}", fixture.name)
            }
        }
    }
}

/// 空文件与「只有 BOM」必须都是空正文，而不是报错或留下 U+FEFF。
#[test]
fn empty_and_bom_only_are_both_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    fs::write(root.join("empty.md"), b"").unwrap();
    fs::write(root.join("bom.md"), b"\xef\xbb\xbf").unwrap();

    for name in ["empty.md", "bom.md"] {
        let note = read_note(root, name).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(note.content, "", "{name} 的正文必须为空");
    }
}
