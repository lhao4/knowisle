//! 文件名规则的**唯一闸门**。
//!
//! 这张表就是验收清单：前端、Tauri 命令、将来的 `isle` CLI 全都收敛到这里。
//! 用例刻意写成"输入 → 期望"，因为这条规则的读者不是编译器，是下一个要加
//! 新入口的人——他要能一眼看出哪些名字已经被拒、理由是什么。

use ki_core::KiError;
use ki_vault::normalize_note_name;

/// 取错误里的 `reason`，顺便断言变体是 `InvalidName`。
///
/// 用变体而不是字符串匹配整条错误：`KiError` 的 `Display` 会带上前缀，
/// 拿它做断言等于把文案格式也锁进测试，改一个字就红。
fn reason_of(input: &str) -> String {
    match normalize_note_name(input) {
        Err(KiError::InvalidName { reason, .. }) => reason,
        Err(other) => panic!("{input} 应当是 InvalidName，实际 {other:?}"),
        Ok(name) => panic!("{input} 应当被拒绝，实际得到 {name}"),
    }
}

/// 验收 8 的七例，一条不少。
#[test]
fn the_seven_rejected_shapes() {
    let long = "a".repeat(300);
    for (input, needle) in [
        ("CON", "保留设备名"),
        ("a:b", "不能有 :"),
        ("foo.", "以点结尾"),
        ("foo ", "空白"),
        ("", "不能为空"),
        ("../x", "不能有 /"),
        (long.as_str(), "太长"),
    ] {
        let reason = reason_of(input);
        assert!(
            reason.contains(needle),
            "{input:?} 的理由应当包含「{needle}」，实际：{reason}"
        );
    }
}

/// 验收 9：全角 `.ｍｄ` 拒绝，且理由要能让用户知道该改成什么。
#[test]
fn fullwidth_extension_is_rejected_with_an_actionable_reason() {
    let reason = reason_of("周会.ｍｄ");
    assert!(reason.contains("全角"), "实际：{reason}");
    assert!(reason.contains(".md"), "理由里必须给出正确写法：{reason}");
}

/// 边界：合法输入必须原样通过，且只补一次后缀。
#[test]
fn accepted_names_are_normalized_once() {
    for (input, expected) in [
        ("周会", "周会.md"),
        ("周会.md", "周会.md"),
        ("周会.markdown", "周会.markdown"),
        ("A.MD", "A.MD"),
        ("会议 2026-10-24", "会议 2026-10-24.md"),
        ("v1.2 发布计划", "v1.2 发布计划.md"),
    ] {
        assert_eq!(normalize_note_name(input).unwrap(), expected, "{input}");
    }
}

/// 目录穿越在**名字层**就被挡住，不必等到 `resolve_for_write`。
///
/// 两层都要有：这一层给出可读理由（"不能有 /"），那一层兜住编码/软链之类的
/// 结构性逃逸。只留一层的话，要么理由难懂，要么规则漏在别的入口上。
#[test]
fn traversal_never_survives_the_name_layer() {
    for input in ["../x", "a/b", "..\\x", "C:\\x", "/etc/passwd"] {
        let reason = reason_of(input);
        assert!(reason.contains("不能有"), "{input} → {reason}");
    }
}

/// 300 字符不是"太长"的唯一形态：中文按字节算，85 个字就到顶。
#[test]
fn length_limit_counts_bytes_not_chars() {
    assert!(
        normalize_note_name(&"字".repeat(84)).is_ok(),
        "84×3+3=255 字节，应当通过"
    );
    assert!(
        normalize_note_name(&"字".repeat(85)).is_err(),
        "85×3+3=258 字节，应当拒绝"
    );
}
