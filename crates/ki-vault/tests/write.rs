//! `write` 模块：原子写的行为与边界。
//!
//! 每个用例对应一个具体的失败场景，不是"覆盖率凑数"。
//! 平台差异与链接夹具见 `tests/common/mod.rs`。

mod common;

use std::fs;
use std::path::Path;

use common::{skip_unless, try_link_dir, try_link_file};
use ki_core::KiError::Io;
use ki_vault::{read_note, scan_vault, write_note, write_note_checked, ScanOptions};
use tempfile::TempDir;

/// 目录里除 `keep` 之外的文件名。用来断言"没有留下垃圾"。
fn entries(vault: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(vault)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn tmp_leftovers(vault: &Path) -> Vec<String> {
    entries(vault)
        .into_iter()
        .filter(|name| name.starts_with(".knowisle.tmp."))
        .collect()
}

/* ───────────────────── 基本写入 ───────────────────── */

/// 目标不存在时必须能创建，且回执是磁盘真值而不是入参长度。
#[test]
fn creates_note_that_did_not_exist() {
    let vault = TempDir::new().unwrap();
    fs::create_dir(vault.path().join("notes")).unwrap();

    let content = "# 新建\n\n第一行。\n";
    let saved = write_note(vault.path(), "notes/new.md", content).expect("新建应当成功");

    assert_eq!(saved.path, "notes/new.md");
    assert_eq!(saved.size, content.len() as u64, "回执大小应等于落盘字节数");
    assert!(saved.mtime > 0, "mtime 应来自磁盘，实际 {}", saved.mtime);

    let read_back = read_note(vault.path(), "notes/new.md").unwrap();
    assert_eq!(read_back.content, content);
    assert_eq!(read_back.size, saved.size);
}

/// 覆盖已有文件：内容必须整份替换，不能残留旧内容的尾巴。
#[test]
fn overwrites_existing_note_without_leaving_a_tail() {
    let vault = TempDir::new().unwrap();
    let long = "长".repeat(5000);
    fs::write(vault.path().join("a.md"), &long).unwrap();

    let short = "短";
    let saved = write_note(vault.path(), "a.md", short).expect("覆盖应当成功");

    assert_eq!(saved.size, short.len() as u64);
    assert_eq!(
        fs::read(vault.path().join("a.md")).unwrap(),
        short.as_bytes()
    );
}

/// 正文必须逐字节原样落盘：不做换行转换、不补末尾换行、不吞制表符。
#[test]
fn round_trips_content_byte_for_byte() {
    let vault = TempDir::new().unwrap();

    let cases = [
        // 中文 + emoji + ZWJ 家族
        ("中文.md", "# 标题\n\n正文 👨‍👩‍👧‍👦 🚀\n"),
        // CRLF 必须原样保留：Windows 用户的笔记就是这样存的
        ("crlf.md", "# A\r\n\r\nB\r\n"),
        // 没有末尾换行
        ("no-trailing.md", "没有末尾换行"),
        // 行首制表符与全角空格
        ("indent.md", "\t缩进\n　全角空格\n"),
        // 只有空白
        ("spaces.md", "   \n\n\t\n"),
    ];

    for (name, content) in cases {
        write_note(vault.path(), name, content)
            .unwrap_or_else(|e| panic!("{name} 写入失败：{e:?}"));
        let read_back = read_note(vault.path(), name).unwrap();
        assert_eq!(read_back.content, content, "{name} 回读不一致");
        assert_eq!(
            fs::read(vault.path().join(name)).unwrap(),
            content.as_bytes(),
            "{name} 磁盘字节不一致"
        );
    }
}

/// 空文件是合法的：新建笔记时先落一个空壳，再逐字输入。
#[test]
fn empty_content_is_allowed() {
    let vault = TempDir::new().unwrap();
    let saved = write_note(vault.path(), "empty.md", "").expect("空内容应当允许");
    assert_eq!(saved.size, 0);
    assert_eq!(read_note(vault.path(), "empty.md").unwrap().content, "");
}

/// 1MB 正文：既是真实笔记的上限量级，也是崩溃测试用的尺寸。
#[test]
fn handles_one_megabyte_of_content() {
    let vault = TempDir::new().unwrap();
    let content = "A".repeat(1024 * 1024);
    let saved = write_note(vault.path(), "big.md", &content).unwrap();
    assert_eq!(saved.size, 1024 * 1024);
    assert_eq!(read_note(vault.path(), "big.md").unwrap().content, content);
}

/// 写不进去的大小就不该被接受：能写不能读是最难排查的不一致。
#[test]
fn rejects_content_larger_than_the_read_limit() {
    let vault = TempDir::new().unwrap();
    let content = "A".repeat(10 * 1024 * 1024 + 1);

    let err = write_note(vault.path(), "huge.md", &content).expect_err("超过读上限应当拒绝");
    assert!(
        format!("{err:?}").contains("TooLarge"),
        "应为 TooLarge，实际 {err:?}"
    );
    assert!(!vault.path().join("huge.md").exists(), "拒绝后不应留下文件");
}

/* ───────────────────── 边界：父目录 ───────────────────── */

/// 父目录不存在必须报错，且**不能代建**——建目录是产品决策，不是写文件的副作用。
#[test]
fn requires_existing_parent_and_never_creates_it() {
    let vault = TempDir::new().unwrap();

    let err = write_note(vault.path(), "no/such/dir/a.md", "x").expect_err("父目录缺失应当报错");
    assert!(
        format!("{err:?}").contains("Io"),
        "应为 IO 错误，实际 {err:?}"
    );

    assert!(!vault.path().join("no").exists(), "不该替用户造目录");
    assert_eq!(
        entries(vault.path()),
        Vec::<String>::new(),
        "目录应保持为空"
    );
}

/// 父目录是指向库外的链接：`canonicalize` 后落在库外，必须拒绝。
#[test]
fn rejects_parent_that_escapes_the_vault() {
    let vault = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();

    skip_unless!(
        try_link_dir(&vault.path().join("escape"), outside.path()),
        "目录链接"
    );

    let err = write_note(vault.path(), "escape/pwn.md", "x").expect_err("父目录逃逸应当拒绝");
    assert!(
        format!("{err:?}").contains("PathEscape"),
        "应为 PathEscape，实际 {err:?}"
    );
    assert!(
        !outside.path().join("pwn.md").exists(),
        "绝不能写到库外：这是最严重的一类失败"
    );
}

/// 目录穿越的语法形态，一个都不能放过。
#[test]
fn rejects_traversal_forms() {
    let vault = TempDir::new().unwrap();
    for rel in ["../a.md", "a/../../b.md", "/etc/passwd", ""] {
        let err = write_note(vault.path(), rel, "x")
            .err()
            .unwrap_or_else(|| panic!("{rel:?} 应当被拒绝"));
        let debug = format!("{err:?}");
        assert!(
            debug.contains("PathEscape") || debug.contains("Io"),
            "{rel:?} 的错误类型异常：{debug}"
        );
    }
}

/* ───────────────────── 边界：最终组件是重解析点 ───────────────────── */

/// `alias.md -> real.md`（都在库内）：必须拒绝。
///
/// 放行的话 `note_save("alias.md")` 会静默改写 `real.md`——用户以为在编辑 A，
/// 实际改坏了 B。这种 bug 比逃逸更难发现，因为两个文件都在库里。
#[test]
fn rejects_symlink_at_final_component() {
    let vault = TempDir::new().unwrap();
    fs::write(vault.path().join("real.md"), "REAL").unwrap();

    skip_unless!(
        try_link_file(
            &vault.path().join("alias.md"),
            &vault.path().join("real.md")
        ),
        "文件链接"
    );

    let err = write_note(vault.path(), "alias.md", "HIJACKED").expect_err("最终组件是软链应当拒绝");
    assert!(
        format!("{err:?}").contains("PathEscape"),
        "应为 PathEscape，实际 {err:?}"
    );
    assert_eq!(
        fs::read_to_string(vault.path().join("real.md")).unwrap(),
        "REAL",
        "被指向的真实文件一个字节都不能动"
    );
}

/// junction 版本的同一件事。Windows 上 junction 不需要管理员权限，
/// 所以这条比文件软链更常在真实环境里出现。
#[test]
fn rejects_junction_at_final_component() {
    let vault = TempDir::new().unwrap();
    let real_dir = vault.path().join("real_dir");
    fs::create_dir(&real_dir).unwrap();

    // junction 只能指向目录，因此这里造一个"目标是目录的链接"，
    // 命中 resolve_for_write 的 is_dir 分支（同样是拒绝，只是理由不同）。
    skip_unless!(
        try_link_dir(&vault.path().join("link_dir"), &real_dir),
        "目录链接"
    );

    let err = write_note(vault.path(), "link_dir", "x").expect_err("最终组件是目录链接应当拒绝");
    // 断言必须是 `PathEscape` 而不是 `Io`：说明命中的是 `is_symlink()` 那道闸，
    // 不是"目标是目录"的兜底。这一点很重要——本机沙箱建不出文件软链，
    // 这条用例是 `rejects_symlink_at_final_component` 在 Windows 上的替代证明。
    assert!(
        format!("{err:?}").contains("PathEscape"),
        "应为 PathEscape（重解析点闸门），实际 {err:?}"
    );
}

/* ───────────────────── tmp 文件的生命周期 ───────────────────── */

/// 成功写入后不能留下 tmp。
#[test]
fn leaves_no_tmp_behind_after_success() {
    let vault = TempDir::new().unwrap();
    write_note(vault.path(), "a.md", "hello").unwrap();
    assert_eq!(tmp_leftovers(vault.path()), Vec::<String>::new());
    assert_eq!(entries(vault.path()), vec!["a.md".to_owned()]);
}

/// 崩溃遗留的孤儿 tmp 不能挡住下一次写入。
///
/// 这正是 kill -9 之后的真实状态：目录里躺着一个 `.knowisle.tmp.<死掉的 pid>.N`。
#[test]
fn orphan_tmp_from_a_crash_does_not_block_the_next_write() {
    let vault = TempDir::new().unwrap();
    let orphan = vault.path().join(".knowisle.tmp.999999.0");
    fs::write(&orphan, "半截内容").unwrap();

    write_note(vault.path(), "a.md", "完整内容").expect("孤儿 tmp 不该影响写入");

    assert_eq!(
        fs::read_to_string(vault.path().join("a.md")).unwrap(),
        "完整内容"
    );
    assert!(orphan.exists(), "本实现不主动清理别人的 tmp");
}

/// tmp 文件不能出现在侧栏里。
///
/// 扫描器只认 `.md` / `.markdown`，所以 `.knowisle.tmp.*` 天然被过滤；
/// 这条用例把"天然"变成"有测试守着"——将来改扩展名规则时会被拦下。
#[test]
fn tmp_files_are_invisible_to_the_scanner() {
    let vault = TempDir::new().unwrap();
    write_note(vault.path(), "a.md", "hello").unwrap();
    fs::write(vault.path().join(".knowisle.tmp.1.0"), "残留").unwrap();

    let scan = scan_vault(vault.path(), &ScanOptions::default()).unwrap();
    assert_eq!(scan.total_files, 1, "只应看到一篇笔记");

    fn collect(node: &ki_core::FileNode, out: &mut Vec<String>) {
        out.push(node.path.clone());
        for child in node.children.iter().flatten() {
            collect(child, out);
        }
    }
    let mut paths = Vec::new();
    collect(&scan.tree, &mut paths);
    assert!(
        !paths.iter().any(|p| p.contains(".knowisle.tmp.")),
        "tmp 泄漏进了树：{paths:?}"
    );
}

/* ───────────────────── 原子性：确定性证明 ───────────────────── */

/// **原子替换的确定性证明**：写之前打开的句柄，写之后读到的仍是旧内容。
///
/// 这是本文件里唯一能**确定地**区分"替换"与"原地重写"的用例，务必留着。
///
/// 原理：`rename` 让旧 inode 与目录项解绑，但已打开的句柄仍然引用旧 inode，
/// 所以它读到的必然是旧内容；而 `fs::write` 是原地截断重写，同一个 inode
/// 内容变了，旧句柄会读到新内容。
///
/// 为什么不能只靠 `crash.rs`：硬杀只能落在**系统调用之间**，1MB 的
/// `write_all` 在本地 SSD 上通常一次 syscall 就进页缓存了，截断窗口只有几十微秒。
/// 也就是说，一个朴素的 `fs::write` 实现有极大概率通过时序型崩溃测试——
/// 这条用例补的正是那个缺口。
#[test]
fn replaces_the_file_instead_of_rewriting_in_place() {
    use std::io::Read;

    let vault = TempDir::new().unwrap();
    let target = vault.path().join("a.md");
    fs::write(&target, "OLD").unwrap();

    // 打开一个贯穿整个写入过程的句柄。
    // Windows 上必须显式允许 delete 共享，否则 rename 会被拒绝——
    // 这本身就是"实现走的是 rename"的一个旁证。
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_SHARE_DELETE: u32 = 0x4;
        opts.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    let mut held = opts.open(&target).expect("打开旧句柄失败");

    write_note(vault.path(), "a.md", "NEW").expect("写入应当成功");

    let mut seen_by_old_handle = String::new();
    held.read_to_string(&mut seen_by_old_handle).unwrap();
    assert_eq!(
        seen_by_old_handle, "OLD",
        "旧句柄读到了新内容 —— 说明是原地重写而不是原子替换"
    );

    assert_eq!(fs::read_to_string(&target).unwrap(), "NEW");
}

/// Windows 上目标被别的句柄占着、且对方**拒绝 delete 共享**时，rename 会失败。
///
/// 这是真实场景：笔记正在别的编辑器（记事本、Word、某些同步盘客户端）里开着。
/// 实测结论：`MoveFileExW` 返回 `ERROR_ACCESS_DENIED`（code 5），
/// **目标保持旧内容**——失败是安全的，但保存会失败。
///
/// 所以 M2 接编辑器时必须把这条错误翻译成"文件被占用，请关闭后重试"，
/// 而不是笼统的"读写失败"。这条用例把行为钉住，防止将来换 API 时悄悄改变。
#[cfg(windows)]
#[test]
fn rename_fails_when_another_handle_denies_delete_sharing() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;

    let vault = TempDir::new().unwrap();
    let target = vault.path().join("a.md");
    fs::write(&target, "OLD").unwrap();

    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&target)
        .unwrap();

    let err = write_note(vault.path(), "a.md", "NEW").expect_err("被占用时应当失败");
    assert!(format!("{err:?}").contains("Io"), "实际 {err:?}");

    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "OLD",
        "失败时目标必须保持旧内容，不能损坏"
    );
    assert_eq!(
        tmp_leftovers(vault.path()),
        Vec::<String>::new(),
        "失败后 tmp 必须被清掉"
    );

    drop(held);
}

/* ───────────────────── std::fs::rename 的实测 ───────────────────── */

/// `write_note` 的原子性完全押在 `fs::rename` 能覆盖已存在文件这件事上。
/// 这一条把那个前提单独测出来：如果哪天换了实现（比如手工 `MoveFileEx`），
/// 这里会先红，而不是等到崩溃测试才发现。
#[test]
fn rename_overwrites_an_existing_file() {
    let dir = TempDir::new().unwrap();
    let tmp = dir.path().join("tmp");
    let target = dir.path().join("target");

    fs::write(&tmp, "NEW").unwrap();
    fs::write(&target, "OLD-CONTENT-IS-LONGER").unwrap();

    fs::rename(&tmp, &target).expect("rename 必须能覆盖已存在的目标文件");

    assert_eq!(fs::read_to_string(&target).unwrap(), "NEW");
    assert!(!tmp.exists(), "源文件应当消失");
}

/// 目标带只读属性时的平台差异，记录行为而不假装统一。
///
/// - Windows：`MoveFileExW(REPLACE_EXISTING)` 遇到只读目标返回 `ACCESS_DENIED`，
///   因此 `write_note` 会失败。**这是安全的失败**：目标保持旧内容，不会损坏。
/// - Unix：rename 只看目录写权限，目标是否只读无关，因此会成功覆盖。
///
/// 不在这里做"先清只读位再写"的兜底：那等于替用户改文件属性，
/// 属于产品决策，不是写文件的职责。
#[test]
fn readonly_target_behaviour_is_platform_specific() {
    let dir = TempDir::new().unwrap();
    let tmp = dir.path().join("tmp");
    let target = dir.path().join("target");

    fs::write(&tmp, "NEW").unwrap();
    fs::write(&target, "OLD").unwrap();
    let mut perms = fs::metadata(&target).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&target, perms).unwrap();

    let result = fs::rename(&tmp, &target);

    #[cfg(windows)]
    {
        assert!(result.is_err(), "Windows 上覆盖只读目标应当失败");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "OLD",
            "失败时目标必须保持旧内容"
        );
    }
    #[cfg(unix)]
    {
        assert!(result.is_ok(), "Unix 上 rename 不受目标只读属性影响");
        assert_eq!(fs::read_to_string(&target).unwrap(), "NEW");
    }
}

/* ───────────────────── 乐观锁：别把别人刚存的改动抹掉 ───────────────────── */

/// 基线正确：mtime 与磁盘一致时，带检查的写必须成功。
#[test]
fn checked_write_succeeds_when_mtime_matches() {
    let vault = TempDir::new().unwrap();
    let first = write_note(vault.path(), "a.md", "V1").unwrap();

    let second = write_note_checked(vault.path(), "a.md", "V2", Some(first.mtime))
        .expect("mtime 一致时应当允许写入");
    assert_eq!(read_note(vault.path(), "a.md").unwrap().content, "V2");
    assert_eq!(second.size, 2);
}

/// 磁盘被外部改过（mtime 变了）时必须拒绝，**并且一个字节都不写**。
///
/// 这条用例是自动保存的安全底线：没有它，知屿会在用户不知情的情况下
/// 覆盖掉 Obsidian 里刚保存的内容。
#[test]
fn checked_write_rejects_a_stale_base_and_leaves_the_file_untouched() {
    let vault = TempDir::new().unwrap();
    let base = write_note(vault.path(), "a.md", "MINE").unwrap();

    // 模拟外部编辑器写入：内容与 mtime 都变。
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(vault.path().join("a.md"), "THEIRS").unwrap();

    let err = write_note_checked(vault.path(), "a.md", "MINE-EDITED", Some(base.mtime))
        .expect_err("基线过期必须报冲突");

    match &err {
        ki_core::KiError::Conflict {
            path,
            expected_mtime,
            disk_mtime,
        } => {
            assert_eq!(path, "a.md");
            assert_eq!(*expected_mtime, base.mtime);
            assert_ne!(*disk_mtime, Some(base.mtime), "磁盘 mtime 应当已经变了");
        }
        other => panic!("应为 Conflict，实际 {other:?}"),
    }

    assert_eq!(
        fs::read_to_string(vault.path().join("a.md")).unwrap(),
        "THEIRS",
        "冲突时绝不能写盘——否则冲突就成了「覆盖之后的马后炮」"
    );
}

/// 目标文件被删掉也算冲突：调用方读到过的文件现在没了，
/// 不能当成「新建」静默写出去（那会让用户以为恢复了一篇其实被别人删掉的笔记）。
#[test]
fn checked_write_reports_conflict_when_the_file_disappeared() {
    let vault = TempDir::new().unwrap();
    let base = write_note(vault.path(), "gone.md", "BYE").unwrap();
    fs::remove_file(vault.path().join("gone.md")).unwrap();

    let err = write_note_checked(vault.path(), "gone.md", "BACK", Some(base.mtime))
        .expect_err("文件消失应当报冲突");
    match &err {
        ki_core::KiError::Conflict { disk_mtime, .. } => {
            assert_eq!(*disk_mtime, None, "文件不存在时 disk_mtime 必须是 None");
        }
        other => panic!("应为 Conflict，实际 {other:?}"),
    }
    assert!(!vault.path().join("gone.md").exists(), "不该凭空重建");
}

/// 不传基线（`None`）时行为与 `write_note` 完全一致：无条件覆盖。
#[test]
fn checked_write_without_a_base_is_an_unconditional_overwrite() {
    let vault = TempDir::new().unwrap();
    write_note(vault.path(), "a.md", "OLD").unwrap();

    write_note_checked(vault.path(), "a.md", "NEW", None).expect("无基线时应当直接覆盖");
    assert_eq!(read_note(vault.path(), "a.md").unwrap().content, "NEW");
}

/* ───────────────────── 错误分类：权限不足 ───────────────────── */

/// 权限不足必须报 `Io { kind: PermissionDenied }`，且**磁盘上的旧内容一个字节不变**。
///
/// 两个平台的可复现构造不同，这不是测试偷懒，是操作系统语义不同：
///
/// - **Windows**：只读属性让「`rename` 覆盖目标」返回 winerror 5（已实测）。
///   文件本身仍可读，所以这条路径恰好验证「写不进去，但旧内容还在」。
/// - **Unix**：`rename` 覆盖只读文件**完全合法**（权限看父目录，不看文件），
///   所以只能把父目录设成 555，让 tmp 文件根本建不出来。
///   若照抄 Windows 的写法，Linux/macOS 上会「测试通过但什么都没验证」。
#[cfg(windows)]
#[test]
// 收尾时要清掉只读位，否则 TempDir 删不掉这个文件。该 lint 的告警是
// 「Unix 上 set_readonly(false) 会变成 world-writable」——本函数整个被
// `#[cfg(windows)]` 圈住，Unix 分支根本不存在，所以这里不适用。
#[allow(clippy::permissions_set_readonly_false)]
fn permission_denied_when_the_target_is_read_only() {
    let vault = TempDir::new().unwrap();
    let target = vault.path().join("a.md");
    fs::write(&target, "旧内容").unwrap();

    let mut perms = fs::metadata(&target).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&target, perms).unwrap();

    let err = write_note(vault.path(), "a.md", "新内容").expect_err("只读目标必须写不进去");
    match err {
        Io { kind, .. } => {
            assert_eq!(kind, ki_core::IoKind::PermissionDenied, "实际 {kind:?}");
        }
        other => panic!("应当是 Io，实际 {other:?}"),
    }

    // 原子写的承诺是「失败时目标不动」，不是「失败时尽量不动」。
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "旧内容",
        "写失败时旧内容必须原封不动"
    );
    assert!(
        tmp_leftovers(vault.path()).is_empty(),
        "失败后不许留下 tmp：{:?}",
        tmp_leftovers(vault.path())
    );

    // 收尾：去掉只读，否则 TempDir 在 Windows 上删不掉这个文件，会留下临时目录。
    let mut perms = fs::metadata(&target).unwrap().permissions();
    perms.set_readonly(false);
    fs::set_permissions(&target, perms).unwrap();
}

/// Unix 版：父目录只读 → 建不出 tmp → `EACCES`。
#[cfg(unix)]
#[test]
fn permission_denied_when_the_parent_directory_is_read_only() {
    use std::os::unix::fs::PermissionsExt;

    let vault = TempDir::new().unwrap();
    let dir = vault.path().join("notes");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("a.md"), "旧内容").unwrap();

    let mut perms = fs::metadata(&dir).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(&dir, perms).unwrap();

    let err = write_note(vault.path(), "notes/a.md", "新内容").expect_err("只读目录必须写不进去");
    match err {
        Io { kind, .. } => {
            assert_eq!(kind, ki_core::IoKind::PermissionDenied, "实际 {kind:?}");
        }
        other => panic!("应当是 Io，实际 {other:?}"),
    }

    assert_eq!(
        fs::read_to_string(dir.join("a.md")).unwrap(),
        "旧内容",
        "写失败时旧内容必须原封不动"
    );

    // 恢复写权限，否则 TempDir 清理不掉。
    let mut perms = fs::metadata(&dir).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dir, perms).unwrap();
}
