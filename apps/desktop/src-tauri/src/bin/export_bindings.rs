//! 生成前端 IPC 契约：`apps/desktop/src/api/bindings.ts`。
//!
//! 用法（在 `apps/desktop` 下）：`pnpm bindings:generate`。
//!
//! # 为什么是 bin 而不是 example
//!
//! 一开始写的是 `examples/export_bindings.rs`——语义上更贴切，而且 `cargo build`
//! 不会顺带编它。但 Windows 上它必然崩在 `STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139)`：
//! `tauri-build` 只把「ComCtl32 v6 应用程序清单」嵌进 **bin** 目标，example 没有清单，
//! 于是 `comctl32.dll` 被当成 v5 加载，而 wry 的导入表里有只有 v6 才有的符号。
//! 换成 bin 目标后清单跟着走，三平台行为一致。
//!
//! # 为什么不放在 `run()` 里按 `debug_assertions` 导出
//!
//! 那是 tauri-specta 的官方示例写法，但它让"生成"变成启动的副作用：
//! 开发时跑一次应用，工作区就多一个脏文件，且没人分得清它是被谁改的。
//! 这里改成显式动作——生成只在被调用时发生，CI 用它做 drift 校验，
//! 开发者用它同步契约。**`run()` 没有任何写文件的副作用。**
//!
//! # 为什么路径用 `CARGO_MANIFEST_DIR`
//!
//! `export()` 的路径参数是相对**进程当前目录**解析的，而 cargo 的 CWD
//! 取决于你在哪儿敲的命令。用编译期常量钉死，从任何目录调用都写到同一个位置
//! ——drift 校验的前提是位置唯一。

fn main() {
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/api/bindings.ts");

    ki_ipc::ipc_builder::<tauri::Wry>()
        .export(specta_typescript::Typescript::default(), &out)
        .expect("导出 bindings.ts 失败");

    println!("bindings.ts -> {}", out.display());
}
