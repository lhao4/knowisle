//! 扫描基准：直接对真实 Vault 跑一次，打印耗时与统计。
//!
//! 用法：
//! ```bash
//! cargo run -p ki-vault --example scan_bench -- /path/to/vault
//! ```
//!
//! 存在的理由：M0 的验收标准是"1000 篇笔记扫描 ≤ 500ms"，
//! 而这个数字必须在真实文件系统上测——单元测试里的 tempdir
//! 走的是页缓存，测出来的数字没有参考价值。

use std::path::PathBuf;
use std::process::ExitCode;

use ki_vault::{scan_vault, ScanOptions};

fn main() -> ExitCode {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("用法: scan_bench <vault 路径>");
        return ExitCode::FAILURE;
    };
    let root = PathBuf::from(arg);

    match scan_vault(&root, &ScanOptions::default()) {
        Ok(result) => {
            println!("root       : {}", result.root);
            println!("files      : {}", result.total_files);
            println!("dirs       : {}", result.total_dirs);
            println!("skipped    : {}", result.skipped);
            println!("truncated  : {}", result.truncated);
            println!("elapsed_ms : {}", result.elapsed_ms);

            // 直接对应 MVP-SCOPE 的 M0 验收线
            let budget_ms = 500;
            if result.elapsed_ms <= budget_ms {
                println!(
                    "\n✅ 扫描耗时 {}ms，在 {}ms 预算内",
                    result.elapsed_ms, budget_ms
                );
                ExitCode::SUCCESS
            } else {
                println!(
                    "\n❌ 扫描耗时 {}ms，超出 {}ms 预算",
                    result.elapsed_ms, budget_ms
                );
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("扫描失败: {err}");
            ExitCode::FAILURE
        }
    }
}
